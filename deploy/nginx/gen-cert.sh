#!/usr/bin/env bash
# Self-signed certificate for the local nginx (slc-mcp HTTPS).
# Installs into /etc/nginx/ssl/slc/ and the system trust store.
# Renewal: re-running overwrites the certificate (nginx reload is not needed
# when paths stay the same — restart ZCode with the new NODE_EXTRA_CA_CERTS).
set -euo pipefail

DIR=/etc/nginx/ssl/slc
DAYS="${CERT_DAYS:-3650}"

sudo mkdir -p "$DIR"
cd /tmp
openssl req -x509 -newkey rsa:2048 -nodes -days "$DAYS" \
  -keyout slc-mcp.key -out slc-mcp.crt \
  -subj "/CN=slc.local/O=SLC-MCP" \
  -addext "subjectAltName=DNS:slc.local,DNS:localhost,IP:127.0.0.1,IP:192.168.0.251" \
  -addext "basicConstraints=CA:FALSE" \
  -addext "keyUsage=digitalSignature,keyEncipherment" \
  -addext "extendedKeyUsage=serverAuth"
sudo mv slc-mcp.key slc-mcp.crt "$DIR/"
sudo chown root:root "$DIR"/*
sudo chmod 600 "$DIR/slc-mcp.key"

# Trust in the system store (curl, browsers, system clients).
sudo cp "$DIR/slc-mcp.crt" /usr/local/share/ca-certificates/slc-mcp.crt
sudo update-ca-certificates

echo "сертификат: $DIR/slc-mcp.crt (SAN: slc.local, localhost, 127.0.0.1, 192.168.0.251)"
echo "для ZCode: NODE_EXTRA_CA_CERTS=$DIR/slc-mcp.crt (уже в zcode.desktop)"
