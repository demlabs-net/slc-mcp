#!/usr/bin/env bash
# Самоподписанный сертификат для локального nginx (slc-mcp HTTPS).
# Устанавливает в /etc/nginx/ssl/slc/ и в системный trust store.
# Продление: повторный запуск перезапишет сертификат (nginx reload не нужен,
# если пути прежние — достаточно перезапуска ZCode с новым NODE_EXTRA_CA_CERTS).
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

# Доверие в системном store (curl, браузеры, системные клиенты).
sudo cp "$DIR/slc-mcp.crt" /usr/local/share/ca-certificates/slc-mcp.crt
sudo update-ca-certificates

echo "сертификат: $DIR/slc-mcp.crt (SAN: slc.local, localhost, 127.0.0.1, 192.168.0.251)"
echo "для ZCode: NODE_EXTRA_CA_CERTS=$DIR/slc-mcp.crt (уже в zcode.desktop)"
