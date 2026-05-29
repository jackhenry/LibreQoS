use crate::lts2_sys::RemoteCommand;
use tracing::{debug, warn};

pub fn start_remote_commands() {
    debug!("Starting remote commands system");
    let _ = std::thread::Builder::new()
        .name("Remote Command Handler".to_string())
        .spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(30));
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
                debug!("Checking for remote commands");

                if crate::lts2_sys::remote_command_count() > 0 {
                    let commands = crate::lts2_sys::remote_commands();
                    process_remote_command_batch(commands, run_command);
                }
            }
        });
}

fn process_remote_command_batch(
    commands: Vec<RemoteCommand>,
    mut run: impl FnMut(RemoteCommand) -> RemoteCommandAction,
) {
    for command in commands {
        if run(command) == RemoteCommandAction::StopBatch {
            break;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RemoteCommandAction {
    Continue,
    StopBatch,
}

fn run_command(command: RemoteCommand) -> RemoteCommandAction {
    run_command_with(
        command,
        crate::lts2_sys::current_capabilities().can_receive_remote_commands,
        crate::program_control::request_graceful_shutdown,
    )
}

fn run_command_with(
    command: RemoteCommand,
    can_receive_remote_commands: bool,
    mut request_shutdown: impl FnMut(&str) -> anyhow::Result<()>,
) -> RemoteCommandAction {
    if !can_receive_remote_commands {
        warn!("Ignoring remote command because current license tier does not permit it");
        return RemoteCommandAction::Continue;
    }
    match command {
        RemoteCommand::Log(msg) => {
            warn!("Message from Insight: {}", msg);
            RemoteCommandAction::Continue
        }
        RemoteCommand::SetInsightControlledTopology { enabled } => {
            if let Ok(config) = lqos_config::load_config() {
                let mut config = (*config).clone();
                config.long_term_stats.enable_insight_topology = Some(enabled);
                if let Err(e) = lqos_config::update_config(&config) {
                    tracing::error!("Failed to update config: {}", e);
                }
                let _ = crate::scheduler_control::enable_scheduler();
                let _ = crate::scheduler_control::restart_scheduler();
            }
            RemoteCommandAction::Continue
        }
        RemoteCommand::SetInsightRole { role } => {
            if let Ok(config) = lqos_config::load_config() {
                let mut config = (*config).clone();
                config.long_term_stats.insight_topology_role = Some(role);
                if let Err(e) = lqos_config::update_config(&config) {
                    tracing::error!("Failed to update config: {}", e);
                }
                let _ = crate::scheduler_control::enable_scheduler();
                let _ = crate::scheduler_control::restart_scheduler();
            }
            RemoteCommandAction::Continue
        }
        RemoteCommand::RestartLqosd => {
            if let Err(err) = request_shutdown("Insight requested lqosd restart") {
                tracing::error!("Unable to request graceful lqosd restart: {err}");
            }
            RemoteCommandAction::StopBatch
        }
        RemoteCommand::RestartScheduler => {
            let _ = crate::scheduler_control::restart_scheduler();
            RemoteCommandAction::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn remote_command_batch_stops_after_restart_request() {
        let commands = vec![
            RemoteCommand::Log("before".to_string()),
            RemoteCommand::RestartLqosd,
            RemoteCommand::Log("after".to_string()),
        ];
        let mut processed = Vec::new();

        process_remote_command_batch(commands, |command| {
            processed.push(command.clone());
            match command {
                RemoteCommand::RestartLqosd => RemoteCommandAction::StopBatch,
                _ => RemoteCommandAction::Continue,
            }
        });

        assert_eq!(
            processed,
            vec![
                RemoteCommand::Log("before".to_string()),
                RemoteCommand::RestartLqosd,
            ]
        );
    }

    #[test]
    fn remote_command_batch_continues_without_restart_request() {
        let commands = vec![
            RemoteCommand::Log("first".to_string()),
            RemoteCommand::RestartScheduler,
            RemoteCommand::Log("last".to_string()),
        ];
        let mut processed = Vec::new();

        process_remote_command_batch(commands.clone(), |command| {
            processed.push(command);
            RemoteCommandAction::Continue
        });

        assert_eq!(processed, commands);
    }

    #[test]
    fn restart_lqosd_requests_shutdown_and_stops_batch() {
        let called = Arc::new(AtomicBool::new(false));
        let called_by_shutdown = called.clone();

        let action = run_command_with(RemoteCommand::RestartLqosd, true, |reason| {
            assert_eq!(reason, "Insight requested lqosd restart");
            called_by_shutdown.store(true, Ordering::SeqCst);
            Ok(())
        });

        assert_eq!(action, RemoteCommandAction::StopBatch);
        assert!(called.load(Ordering::SeqCst));
    }

    #[test]
    fn restart_lqosd_does_not_shutdown_without_remote_command_capability() {
        let called = Arc::new(AtomicBool::new(false));
        let called_by_shutdown = called.clone();

        let action = run_command_with(RemoteCommand::RestartLqosd, false, |_| {
            called_by_shutdown.store(true, Ordering::SeqCst);
            Ok(())
        });

        assert_eq!(action, RemoteCommandAction::Continue);
        assert!(!called.load(Ordering::SeqCst));
    }
}
