# Records docs/cli.gif with VHS 0.11: a search and a download from the command
# line. Run it from the repository root after building the release binary with
# `cargo build --release -p torrential`:
#
#   vhs docs/tapes/cli.tape
#
# It downloads the 58 MiB MP4 of Big Buck Bunny (CC BY 3.0) from archive.org
# into /tmp and deletes it at the end.

Output docs/cli.gif
Source docs/tapes/theme.tape
Set FontSize 16
Set Width 1096
Set Height 520
Set Padding 16
Set Margin 18
Set WindowBarSize 36
Set BorderRadius 8
Set Framerate 24
Set TypingSpeed 50ms
Set PlaybackSpeed 1.5
Set WaitTimeout 90s
Set WaitPattern /^\$$/

Hide
Type ". docs/tapes/setup.sh"
Enter
Sleep 1s
Show

Type "torrential search big buck bunny --limit 5"
Sleep 400ms
Enter
Wait
Sleep 3.5s

Type "torrential get fd69b2 --files '*h264.mp4'"
Sleep 400ms
Enter
Wait
Sleep 3.5s

Hide
Type "torrential engine stop; rm -rf /tmp/torrential-demo"
Enter
Sleep 2s
