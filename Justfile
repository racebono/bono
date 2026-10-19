[windows]
set shell := ["powershell.exe", "-NoLogo", "-Command"]

run:
    cargo build -p bono-phonemizer
    cargo run
