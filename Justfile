[windows]
set shell := ["powershell.exe", "-NoLogo", "-Command"]

run:
    cargo build -p lambiase-phonemizer
    cargo run
