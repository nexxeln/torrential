# Records docs/tui.gif with VHS 0.11: a search, a file pick, and a download in
# the terminal UI. Run it from the repository root after building the release
# binary with `cargo build --release -p torrential`:
#
#   vhs docs/tapes/tui.tape
#
# It downloads the 58 MiB MP4 of Big Buck Bunny (CC BY 3.0) from archive.org
# into /tmp and deletes it at the end.

Output docs/tui.gif
Source docs/tapes/theme.tape
Set FontSize 16
Set Width 1096
Set Height 700
Set Padding 16
Set Margin 18
Set WindowBarSize 36
Set BorderRadius 8
Set Framerate 24
Set TypingSpeed 90ms
Set PlaybackSpeed 1.8
Set WaitTimeout 90s

Hide
Type ". docs/tapes/setup.sh"
Enter
Sleep 1s
Show

Type "torrential"
Sleep 300ms
Enter
Sleep 800ms
Type "big buck bunny"
Sleep 300ms
Enter
Wait+Screen /ranked for/
Sleep 3s

Enter
Wait+Screen /of 10 files/
Sleep 2s
Type "a"
Sleep 500ms
Type "jjjjjjj"
Sleep 300ms
Space
Sleep 1.8s

Enter
Sleep 800ms
Type "d"
Wait+Screen /done/
Type "r"
Sleep 2s

Hide
Type "q"
Sleep 500ms
Type "torrential engine stop; rm -rf /tmp/torrential-demo"
Enter
Sleep 2s
