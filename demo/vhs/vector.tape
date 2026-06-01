Output demo/vhs/vector-demo.gif
Set FontSize 18
Set Width 1100
Set Height 720
Set Theme "Catppuccin Mocha"

Type "docker compose up -d rustfs"
Enter
Sleep 1s

Type "cargo run -p marila"
Enter
Sleep 1s
Ctrl+C

Type "PARLIS_DIR=~/parlis/pdfs MAX_CHUNKS=20000 bash demo/vector/index.sh"
Enter
Sleep 1s

Type "cd demo && uv run python -m vector.chat"
Enter
Sleep 1s

Type "find the strongest arguments in the latest corpus"
Sleep 1s
