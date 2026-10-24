#!/bin/sh
export SCARLET_HOME_TRACE=1
/bin/tabletctl posture tablet
/bin/tabletctl windowing auto
/bin/scarlet-desktop --shell-mode desktop > /tmp/home-trace-shell.log
