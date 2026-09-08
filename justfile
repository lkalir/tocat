test:
    sudo capsh \
    --caps="cap_net_admin+eip cap_setpcap,cap_setuid,cap_setgid+ep" \
    --keep=1 \
    --user="$USER" \
    --addamb=cap_net_admin \
    -- -c 'cargo nextest run --workspace --locked --profile ci'    

fmt:
    cargo-nightly fmt

clippy:
    cargo-nightly clippy --workspace --all-targets --all-features
