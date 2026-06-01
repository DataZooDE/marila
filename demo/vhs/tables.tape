Output demo/vhs/tables-demo.gif
Set FontSize 18
Set Width 1100
Set Height 720
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

Type "printf '%s\n' 'Tables demo: boto3 s3tables control plane' 'DuckDB Iceberg REST data plane -> /iceberg/v1/...' ''"
Enter
Sleep 1s

Type "printf '%s\n' '$ docker compose --profile lakekeeper up -d' '$ cargo run -p marila' '$ bash demo/tables/load.sh' '' 'load.sh creates the table bucket, namespace, and table with boto3 s3tables.' 'DuckDB then attaches marila /iceberg/v1 and reads/writes Iceberg data.'"
Enter
Sleep 1s

Type "printf '%s\n' '$ cd demo && uv run python -m tables.chat' 'you> trips per pickup borough by day of week' '  -> schema_lookup' '  -> pivot rows=pickup_borough cols=day_of_week measure=trip_count'"
Enter
Sleep 1s

Type "Result: compact pivot table in the TUI, with SQL and query trace."
Sleep 1s
