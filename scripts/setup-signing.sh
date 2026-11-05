#!/bin/sh
# Creates a local, self-signed code-signing certificate named
# "canvas-cli local" in your login keychain, so `make install` can sign
# canvas and canvas-mcp with a stable identity.
#
# Why: macOS remembers "Always Allow" for a keychain item per program
# signature. Unsigned builds change identity on every rebuild, so macOS
# asks again. Signed with this certificate, every build is the same app.
#
# Nothing leaves this machine, and no password or Apple account is needed.
# Run once:  make setup-signing
set -eu

NAME="canvas-cli local"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-certificate -c "$NAME" "$KEYCHAIN" >/dev/null 2>&1; then
  echo "Signing identity \"$NAME\" already exists."
  exit 0
fi

DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT

cat > "$DIR/cert.conf" <<CONF
[req]
distinguished_name = dn
prompt = no
x509_extensions = ext
[dn]
CN = $NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CONF

openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout "$DIR/key.pem" -out "$DIR/cert.pem" -config "$DIR/cert.conf" 2>/dev/null

# A random one-time password only protects the temporary .p12 file.
PASS=$(openssl rand -hex 16)
openssl pkcs12 -export -inkey "$DIR/key.pem" -in "$DIR/cert.pem" \
  -name "$NAME" -out "$DIR/id.p12" -passout "pass:$PASS" 2>/dev/null

security import "$DIR/id.p12" -k "$KEYCHAIN" -P "$PASS" -T /usr/bin/codesign >/dev/null

# No trust setting is needed: the keychain matches the signature by this
# certificate's own hash, not by a trusted authority.

echo "Created \"$NAME\". Now run: make install"
