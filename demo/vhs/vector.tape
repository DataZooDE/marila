Output demo/vhs/vector-demo.gif
Set FontSize 18
Set Width 1100
Set Height 720
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

Type "printf '%s\n' 'Vector demo: boto3 s3vectors -> endpoint_url=http://localhost:8080' 'No private Rust hooks; the TUI talks AWS protocol.' ''"
Enter
Sleep 1s

Type "printf '%s\n' '$ docker compose up -d rustfs' '$ cargo run -p marila' '$ PARLIS_DIR=~/parlis/pdfs MAX_CHUNKS=20000 bash demo/vector/index.sh' '' 'index.sh creates a vector bucket with aws s3vectors.' 'marila-embed chunks PDFs and writes vectors through aws-sdk-s3vectors.'"
Enter
Sleep 1s

Type "printf '%s\n' '$ cd demo && uv run python -m vector.chat' 'you> find the strongest arguments in the latest corpus' '  -> boto3 s3vectors query' '  -> ranked source chunks + cited answer'"
Enter
Sleep 1s

Type "Result: chat pane, retrieval hops, and source preview."
Sleep 1s
