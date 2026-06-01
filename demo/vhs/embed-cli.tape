Output demo/vhs/embed-cli.gif
Set FontSize 18
Set Width 1100
Set Height 720
Set Theme "Catppuccin Mocha"

Type "cargo run -p marila-embed -- put --help"
Enter
Sleep 1s

Type "cargo run -p marila-embed -- query --help"
Enter
Sleep 1s

Type "cargo run -p marila-embed -- put --vector-bucket-name parlis --index-name drucksachen --embedding-provider ollama --embedding-model embeddinggemma:latest --text ./docs --include pdf"
Sleep 1s
