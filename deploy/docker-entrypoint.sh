#!/bin/sh
# Container entrypoint. The working folder /data is the install folder (.env, database.sqlite,
# uploads/, themes/). On first start the default theme is copied there; existing themes are
# never overwritten. Then the binary replaces this shell, so it is PID 1 and gets SIGTERM.
set -eu
if [ ! -d themes ]; then
  # Copy under a temporary name, then rename: an interrupted copy never leaves a half theme
  # folder that the check above would accept.
  rm -rf ./themes.tmp
  cp -R /usr/share/mission-control/themes ./themes.tmp
  mv ./themes.tmp ./themes
fi
exec /usr/local/bin/mission-control-app "$@"
