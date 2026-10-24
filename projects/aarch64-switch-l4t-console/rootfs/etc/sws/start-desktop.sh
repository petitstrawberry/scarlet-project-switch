#!/bin/sh
# Set board presentation policy before the supervised desktop shell starts.
/bin/tabletctl posture tablet
/bin/tabletctl windowing auto
/bin/scarlet-desktop --shell-mode desktop
