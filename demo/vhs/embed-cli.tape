Output demo/vhs/embed-cli.gif
Set FontSize 18
Set Width 1100
Set Height 720
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

Type "printf '%s\n' 'marila-embed: AWS-aligned S3 Vectors indexing CLI' '' 'Uses aws-sdk-s3vectors + endpoint_url=http://localhost:8080' ''"
Enter
Sleep 1s

Type "printf '%s\n' '$ marila-embed put --vector-bucket-name parlis --index-name drucksachen --text ./docs --include pdf' '  -> parse files, chunk text, embed, PutVectors via AWS SDK' '' '$ marila-embed query --vector-bucket-name parlis --index-name drucksachen --text-value summarize-the-corpus --k 5' '  -> embed query text, QueryVectors via AWS SDK'"
Enter
Sleep 1s

Type "cargo run -p marila-embed -- put --vector-bucket-name parlis --index-name drucksachen --embedding-provider ollama --embedding-model embeddinggemma:latest --text ./docs --include pdf"
Sleep 1s
