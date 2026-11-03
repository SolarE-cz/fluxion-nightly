# FluxION Fleet Reporter

Reads Home Assistant and posts one signed `legacy` heartbeat to the FluxION
fleet register every `interval_s` seconds. It never calls a Home Assistant
service and never writes to Home Assistant.

Set `ingest_url` to the register and paste the one-time claim code into
`claim_code`. The code is used once and is never logged. A refused code is
retried every 10 minutes.

The reporter sends nothing unless `ingest_url` is https, or plain HTTP to a loopback, private-network (RFC 1918, CGNAT such as NetBird or Tailscale, link-local) or `.local`, `.netbird.cloud` or `.ts.net` host.

## What it reads

Only the entities named in the options: `pv_power_entity`, `soc_entity`,
`import_today_entity`, `export_today_entity`, `plan_summary_entity`,
`fluxion_version_entity` and `fluxion_alive_entity`, plus their unit and version
attributes.

`fluxion_alive_entity` tells the register whether FluxION itself runs. FluxION
refreshes it every few seconds, so the heartbeat reports `fallback` `none` while
its last update is within 10 minutes, `stale` when it is older, and `unknown`
when the option is empty or the entity is missing.
`plan_summary_entity` defaults to the active FluxION work mode; an empty value
sends no plan summary.

## Where the key lives

One Ed25519 identity per site, in `/data/identity.json`, mode `0600`. The
private key never leaves the site. The file is excluded from Home Assistant
backups. A restart reuses the same key.

## After a lost or corrupt key

Delete `/data/identity.json`, ask the register to reissue the claim code
(`reissue_claim_code`), paste it into `claim_code` and start the add-on. The
site row stays; only the key is new.

Development: `FLUXION_REPORTER_FIXTURE` reads a JSON snapshot instead of a
live site; `--once --dry-run` prints the signed request without sending it.
