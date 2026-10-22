# FluxION Fleet Reporter

Reads Home Assistant and posts one signed `legacy` heartbeat to the FluxION
fleet register every `interval_s` seconds. It never calls a Home Assistant
service and never writes to Home Assistant.

Set `ingest_url` to the register and paste the one-time claim code into
`claim_code`. The code is used once and is never logged. A refused code is
retried every 10 minutes.

## What it reads

Only the entities named in the options: `pv_power_entity`, `soc_entity`,
`import_today_entity`, `export_today_entity`, `plan_summary_entity` and
`fluxion_version_entity`, plus their unit and version attributes.

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
