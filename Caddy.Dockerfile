# Plain Caddy -- no custom build, no ACME DNS plugin. The origin's cert is issued
# CORE-side (CADS-Tunnel's scripts/authorize-pipeline.sh, deSEC DNS-01) and mounted
# in as static files; Caddy here only ever reads fullchain.pem/privkey.pem -- it
# never holds the deSEC zone-wide token. Same convention as the other CADS-Tunnel
# demos (help-site, flappy-demo, cookbook-demo).
FROM caddy:2
