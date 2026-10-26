#!/bin/sh
# Signs the macOS stfgd and stfg binaries in place, then verifies them.
# Usage: packaging/macos/sign.sh <dir containing stfgd and stfg>
#   CODESIGN_IDENTITY  SHA-1 of a self-signed code-signing certificate in the keychain search
#                      list (default: empty, ad-hoc signature). See docs/distribution.md#signing.
#
# A certificate signature makes the designated requirement `identifier … and certificate leaf =
# H"<sha1>"`, which stays the same across releases, so TCC grants (Full Disk Access, folder
# access) and keychain "Always Allow" entries given to stfgd survive updates. An ad-hoc one pins
# the cdhash and changes with every build.
set -eu

if [ "$#" -ne 1 ] || [ ! -d "$1" ]; then
    echo "usage: $0 <dir containing stfgd and stfg>" >&2
    exit 2
fi
DIR="$1"
ID_PREFIX="ua.SkeLLLa.star-forge"

SIGN_ID="${CODESIGN_IDENTITY:-}"
if [ -n "$SIGN_ID" ]; then
    case "$SIGN_ID" in
        *[!0-9A-Fa-f]*) SIGN_ID="" ;;
    esac
    if [ "${#SIGN_ID}" -ne 40 ]; then
        echo "error: CODESIGN_IDENTITY must be the certificate's 40-digit SHA-1" >&2
        exit 1
    fi
    SIGN_ID="$(printf %s "$SIGN_ID" | tr '[:lower:]' '[:upper:]')"
else
    SIGN_ID="-"
fi

sign() { # <identifier> <path>
    if [ "$SIGN_ID" = "-" ]; then
        codesign --force --options runtime --timestamp=none --sign - -i "$1" "$2"
    else
        # The requirement is spelled out rather than left to codesign's default, so it can't drift.
        codesign --force --options runtime --timestamp=none --sign "$SIGN_ID" -i "$1" \
            -r="designated => identifier \"$1\" and certificate leaf = H\"$SIGN_ID\"" "$2"
    fi
}

# Fail unless both the signature and the designated requirement pin this identifier and leaf
# (catches a wrong key, or a requirement that fell back to the cdhash).
check_pinned() { # <identifier> <path>
    codesign --verify --strict -R="identifier \"$1\" and certificate leaf = H\"$SIGN_ID\"" "$2"
    codesign -d -r- "$2" 2>&1 | grep -qi "identifier \"$1\" and certificate leaf = H\"$SIGN_ID\"" || {
        echo "error: the designated requirement of $2 does not pin leaf $SIGN_ID" >&2
        exit 1
    }
}

for bin in stfgd stfg; do
    test -f "$DIR/$bin" || { echo "error: $DIR/$bin is missing" >&2; exit 1; }
    sign "$ID_PREFIX.$bin" "$DIR/$bin"
    codesign --verify --strict "$DIR/$bin"
    if [ "$SIGN_ID" != "-" ]; then
        check_pinned "$ID_PREFIX.$bin" "$DIR/$bin"
    fi
done
