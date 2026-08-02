#!/usr/bin/env bash
# Create the Android release signing key and hand it to GitHub Actions.
#
# This is deliberately NOT part of CI. A signing key is the one artefact that
# must never be regenerated: an APK signed with a different key cannot upgrade
# the installed app, Android refuses it outright, and the only way forward is an
# uninstall that takes the app's data with it. A workflow that could mint a key
# is a workflow that can silently brick every phone running the app, so the key
# is made once, by hand, and backed up by a person who understands that.
#
# Everything *around* that -- encoding it, pushing four secrets, checking they
# landed -- is ceremony, and ceremony belongs in a script.
#
# Usage:  scripts/setup-signing.sh [--repo owner/name] [--keystore path]
set -euo pipefail

REPO="ErikFrankling/time"
KEYSTORE="$HOME/time-release.jks"

while [ $# -gt 0 ]; do
  case "$1" in
    --repo)     REPO="$2"; shift 2 ;;
    --keystore) KEYSTORE="$2"; shift 2 ;;
    -h|--help)  sed -n '2,16p' "$0" | sed 's/^# \?//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

need() { command -v "$1" >/dev/null || { echo "missing: $1" >&2; exit 1; }; }

# keytool ships with the JDK, which is in the dev shell rather than on the
# system. Saying so beats leaving someone to work out where a JDK went.
command -v keytool >/dev/null || {
  cat >&2 <<'EOF'
keytool not found -- it comes with the JDK, which lives in the dev shell.

Run this instead, from the repository root:
  nix develop --command scripts/setup-signing.sh
EOF
  exit 1
}
need gh
need openssl
need base64

gh auth status >/dev/null 2>&1 || {
  echo "gh is not logged in. Run: gh auth login" >&2
  exit 1
}

# Refusing to overwrite is the whole point. If a key already exists, either it
# is the one the installed app trusts -- in which case replacing it breaks
# updates -- or it is abandoned, and the person running this should say so.
if [ -e "$KEYSTORE" ]; then
  cat >&2 <<EOF
A keystore already exists at $KEYSTORE

Not touching it. If the app in the wild was signed with it, replacing it makes
every future update fail with INSTALL_FAILED_UPDATE_INCOMPATIBLE.

If you are certain it is unused, move it aside and run this again:
  mv $KEYSTORE $KEYSTORE.old
EOF
  exit 1
fi

echo "Generating a 4096-bit RSA key valid for 27 years..."
STOREPASS="$(openssl rand -base64 24)"

# PKCS12 rather than the deprecated JKS, and one password for store and key:
# Gradle's signingConfig has no way to express two, and a second secret would
# buy nothing over the first.
keytool -genkeypair \
  -keystore "$KEYSTORE" -storetype PKCS12 \
  -storepass "$STOREPASS" -keypass "$STOREPASS" \
  -alias time -keyalg RSA -keysize 4096 -validity 10000 \
  -dname "CN=time, O=frankling.se, C=SE" >/dev/null

chmod 600 "$KEYSTORE"
echo "Wrote $KEYSTORE"

echo "Pushing secrets to $REPO..."
base64 -w0 "$KEYSTORE" | gh secret set KEYSTORE_BASE64  --repo "$REPO"
gh secret set KEYSTORE_PASSWORD --repo "$REPO" --body "$STOREPASS"
gh secret set KEY_PASSWORD      --repo "$REPO" --body "$STOREPASS"
gh secret set KEY_ALIAS         --repo "$REPO" --body time

# Setting a secret can succeed against the wrong repository, or against a fork,
# and the failure would only show up as an unsigned build much later.
missing=""
for s in KEYSTORE_BASE64 KEYSTORE_PASSWORD KEY_PASSWORD KEY_ALIAS; do
  gh secret list --repo "$REPO" 2>/dev/null | grep -q "^$s" || missing="$missing $s"
done
[ -z "$missing" ] || { echo "secrets did not land:$missing" >&2; exit 1; }
echo "All four secrets present."

cat <<EOF

──────────────────────────────────────────────────────────────────────────
Store this password somewhere durable. It is not recoverable, and without
it the key is useless -- which means no further updates, ever.

    $STOREPASS

Back up $KEYSTORE too. Same reason.
──────────────────────────────────────────────────────────────────────────

Next:
  gh workflow run android.yml --repo $REPO

That publishes a release-signed APK. The debug build currently on the phone
was signed with a throwaway key and cannot be upgraded to it, so uninstall it
first, then install once from Obtainium:

    https://github.com/$REPO

After that every update is a notification and a tap.
EOF
