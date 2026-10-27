#!/usr/bin/env bash
# Builds the CouchTV Remote APK with plain SDK tools (no Gradle) and copies it to
# ~/.local/share/couchtv so the PC serves it at http://<pc>:7777/couchtv.apk.
set -euo pipefail
cd "$(dirname "$0")"

SDK=${ANDROID_HOME:-$HOME/Android/Sdk}
BT=$SDK/build-tools/36.0.0
JAR=$SDK/platforms/android-36/android.jar
OUT=build
VERSION_CODE=${VERSION_CODE:-$(date +%s | cut -c1-9)}

rm -rf "$OUT"
mkdir -p "$OUT/gen" "$OUT/classes" "$OUT/dex"

"$BT/aapt2" compile --dir res -o "$OUT/res.zip"
"$BT/aapt2" link -o "$OUT/unsigned.apk" -I "$JAR" --manifest AndroidManifest.xml -A assets \
  --java "$OUT/gen" --min-sdk-version 30 --target-sdk-version 36 \
  --version-code "$VERSION_CODE" --version-name 1.0 "$OUT/res.zip"
javac --release 17 -nowarn -Xlint:-options -classpath "$JAR" -d "$OUT/classes" $(find src "$OUT/gen" -name '*.java')
"$BT/d8" --release --min-api 30 --lib "$JAR" --output "$OUT/dex" $(find "$OUT/classes" -name '*.class')
(cd "$OUT/dex" && zip -q ../unsigned.apk classes.dex)
"$BT/zipalign" -f -p 4 "$OUT/unsigned.apk" "$OUT/aligned.apk"

KS=$HOME/.android/debug.keystore
"$BT/apksigner" sign --ks "$KS" --ks-pass pass:android --key-pass pass:android \
  --out "$OUT/couchtv-remote.apk" "$OUT/aligned.apk"

DEST=${XDG_DATA_HOME:-$HOME/.local/share}/couchtv
mkdir -p "$DEST"
cp "$OUT/couchtv-remote.apk" "$DEST/"
echo "Built $(pwd)/$OUT/couchtv-remote.apk"
