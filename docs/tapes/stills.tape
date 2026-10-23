# Takes docs/search.png, docs/files.png, and docs/ranking.png with VHS 0.11, at
# twice the size of the GIFs so they stay sharp on high-density screens. Run it
# from the repository root after building the release binary with
# `cargo build --release -p torrential`:
#
#   vhs docs/tapes/stills.tape
#
# It searches archive.org and reads one file list, and downloads nothing.

Source docs/tapes/theme.tape
Set FontSize 32
Set Width 2120
Set Height 1368
Set Padding 32
Set Margin 36
Set WindowBarSize 72
Set BorderRadius 16
Set TypingSpeed 0ms
Set WaitTimeout 90s

Hide
Type ". docs/tapes/setup.sh"
Enter
Sleep 1s
Type "torrential"
Enter
Sleep 1s
Type "night of the living dead"
Enter
Wait+Screen /ranked for/
Wait+Screen /· \d+ files/
Show
Sleep 500ms
Screenshot docs/search.png

Enter
Wait+Screen /of 5 files/
Type "ajj"
Space
Sleep 500ms
Screenshot docs/files.png

Escape
Type "S"
Sleep 500ms
Screenshot docs/ranking.png

Hide
Escape
Type "q"
Sleep 500ms
Type "torrential engine stop; rm -rf /tmp/torrential-demo"
Enter
Sleep 2s
