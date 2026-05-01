#!/bin/bash
set -euo pipefail

VENV_DIR="/opt/libreqos/venv"
REQUIREMENTS="/opt/libreqos/src/requirements.txt"
CONSTRAINTS="/opt/libreqos/src/deb-requirements-constraints.txt"

if [ "$(id -u)" -ne 0 ]; then
  echo "rebuild_python_venv.sh must be run as root." >&2
  exit 1
fi

if [ ! -f "$REQUIREMENTS" ]; then
  echo "LibreQoS Python requirements file not found: $REQUIREMENTS" >&2
  exit 1
fi

python3 -m venv --clear "$VENV_DIR"
"$VENV_DIR/bin/python" -m pip install --upgrade pip

if [ -s "$CONSTRAINTS" ]; then
  "$VENV_DIR/bin/python" -m pip install --upgrade -c "$CONSTRAINTS" -r "$REQUIREMENTS"
else
  "$VENV_DIR/bin/python" -m pip install --upgrade -r "$REQUIREMENTS"
fi

chown -R root:root "$VENV_DIR"
chmod -R go-w "$VENV_DIR"
