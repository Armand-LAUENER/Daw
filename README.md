# Daw

Moteur audio temps réel en Rust, avec pour but une station audio utilisable et
performante. Chaque phase a un critère de sortie mesurable — on ne passe à la
suivante qu'une fois le critère vérifié par une mesure, pas par une impression.

**Règle centrale, non négociable** : le thread audio (callback `cpal`) ne fait jamais
d'allocation, de lock, d'I/O ni d'attente bloquante. Toute communication avec ce thread
passe par des structures lock-free. C'est outillé, pas seulement documenté :
`assert_no_alloc` en debug et le test `crates/engine-io/tests/no_alloc.rs` font échouer
la CI si la règle est violée.

## État

**Phase 0 terminée et validée.** Sinus temps réel piloté par un thread de contrôle :
10 min puis 30 min sans xrun en natif Windows (WASAPI), aucun clic audible aux
changements de note.

**Prochaine étape : phase 1** — lecture de fichier et filtre.

Le plan détaillé et le journal de bord sont dans [`plan-daw-rust.md`](plan-daw-rust.md).
L'état courant et les pièges d'environnement déjà rencontrés sont dans
[`docs/REPRISE.md`](docs/REPRISE.md) — à lire en premier en reprenant le projet.

## Structure

```
crates/
├── engine-dsp/   DSP pur, sans I/O, testable sans carte son
│   ├── oscillator.rs   SineOsc : phase f64 normalisée, continue aux changements de fréquence
│   └── smoother.rs     LinearSmoother : rampe dérivée de la cible (pas d'accumulation f32)
└── engine-io/    cpal + thread audio
    ├── processor.rs    SineProcessor : tout ce que le callback possède, indépendant de cpal
    ├── stream.rs       AudioEngine : ouverture du device, EngineOptions::validate
    ├── diagnostics.rs  Stats (atomiques), XrunDetector (timestamps), DriftMonitor (starvation)
    ├── control.rs      Command (Copy, sans tas) : SetFrequency, SetGain
    └── examples/phase0.rs   driver + test de charge (exit 1 si xrun)
```

Les commandes vers le thread audio et les `RtEvent` qui en reviennent passent par des
ring buffers SPSC (`rtrb`), complétés par des compteurs atomiques `Stats`. Les commandes
invalides (NaN, inf) sont rejetées et comptées dans `commands_rejected` ; les options
invalides sont refusées en amont par `EngineOptions::validate`.

## Démarrage

```bash
git clone https://github.com/Armand-LAUENER/Daw.git && cd Daw
git config core.hooksPath .githooks   # à refaire après chaque clone
scripts/check.sh                      # fmt, clippy, tests debug + release, rustdoc
```

Lancer le driver phase 0 :

```bash
cargo run --release -p engine-io --example phase0 -- --list
cargo run --release -p engine-io --example phase0 -- --minutes 30 --gain 0.05
```

```
usage: phase0 [--list] [--minutes N | --seconds N] [--change-ms MS]
              [--report-s S] [--buffer FRAMES] [--host NAME] [--device SUBSTR] [--gain 0..1]
```

> **Pas de validation temps réel sous WSL.** L'audio de WSLg décroche systématiquement
> vers 70-80 s, y compris avec `paplay` — donc indépendamment de ce code. Sous WSL,
> s'en tenir aux tests hors temps réel, et utiliser `--features pulseaudio --host pulseaudio`
> pour éviter que le plugin ALSA `pulse` n'inonde le callback d'erreurs.

## Outils

| Commande | Rôle |
|---|---|
| `scripts/check.sh` | Tout ce que fait la CI, par coût croissant. `--fast` s'arrête après les lints. |
| `scripts/windows.sh test\|check` | Les mêmes vérifications **sous Windows**, lancées depuis WSL, sur un instantané de l'arbre de travail (y compris le non commité). |
| `scripts/windows.sh phase0 --minutes 30 --gain 0.05` | Run temps réel sur la vraie sortie audio Windows. Log dans `target/windows-logs/`. |
| `.githooks/` | `pre-commit` → `check.sh --fast` ; `pre-push` → `check.sh`. |
| `.github/workflows/ci.yml` | `check.sh` sur Linux et Windows. |

Tests unitaires dans chaque module, tests de propriétés `proptest` dans
`crates/*/tests/properties.rs`. Les fichiers `*.proptest-regressions` sont commités,
pour que les cas ayant déjà trouvé un bug soient rejoués à chaque exécution.

## Licence

MIT OR Apache-2.0.
