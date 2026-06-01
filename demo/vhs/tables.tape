Output demo/vhs/tables-demo.gif
Set FontSize 18
Set Width 1100
Set Height 720
Set Theme "Catppuccin Mocha"

Type "docker compose --profile lakekeeper up -d"
Enter
Sleep 1s

Type "cargo run -p marila"
Enter
Sleep 1s
Ctrl+C

Type "bash demo/tables/load.sh"
Enter
Sleep 1s

Type "cd demo && uv run python -m tables.chat"
Enter
Sleep 1s

Type "trips per pickup borough by day of week"
Sleep 1s
