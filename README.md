# canari

Monitoring de type « dead man's switch » pour cron jobs et services de fond :
un job qui cesse de pinguer déclenche une alerte. Clone minimal de
[healthchecks](https://github.com/healthchecks/healthchecks), en un seul
binaire, sans dépendance externe (SQLite est compilé dans le binaire).

## Lancer

```sh
cargo run -- --db canari.db --listen 127.0.0.1:8000 --site-url https://canari.example.org
```

| Option | Variable | Défaut |
| --- | --- | --- |
| `--db` | `CANARI_DB` | `canari.db` |
| `--listen` | `CANARI_LISTEN` | `127.0.0.1:8000` |
| `--site-url` | `CANARI_SITE_URL` | `http://localhost:8000` |

Verbosité des logs : `CANARI_LOG=canari=debug` (syntaxe `tracing` / `RUST_LOG`).

## État

Étape 1 — squelette : serveur, base SQLite (migrations appliquées au
démarrage), configuration, arrêt gracieux, `GET /healthz`.

À suivre : ingestion des pings, moteur d'état et boucle d'alerte, canaux de
notification (webhook puis ntfy), UI, API + badges.

## Notes de conception

- **Horodatages** : entiers, epoch UTC. Les comparaisons `alert_after <= now`
  restent exactes et indexables.
- **`checks.alert_after`** : instant à partir duquel un check est en retard.
  La boucle d'alerte ne scanne que cette colonne (index partiel sur les statuts
  `up`/`grace`) au lieu de recalculer les plannings à chaque tick.
- **SQLite** : mode WAL + `busy_timeout` — les lectures ne bloquent pas, les
  écritures concurrentes s'attendent au lieu d'échouer en `SQLITE_BUSY`.
- **`paused` est un statut**, pas un drapeau : un check en pause n'est jamais
  en retard, donc il sort naturellement de la requête d'alerte.
