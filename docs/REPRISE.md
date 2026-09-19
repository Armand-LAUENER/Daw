# Reprise — état du projet au 2026-09-19

À lire en premier en reprenant le projet. Le plan complet et le journal de bord
sont dans [`plan-daw-rust.md`](../plan-daw-rust.md).

## Où on en est

**Phase 0 terminée et validée** (commit `e33d571`) : sinus temps réel piloté par
un thread de contrôle, 10 min puis 30 min sans xrun en natif Windows (WASAPI),
aucun clic audible aux changements de note.

**Prochaine étape : phase 1** (lecture de fichier + filtre). Le plan d'attaque
est prêt, mais trois décisions restent à prendre (voir en bas).

## Architecture actuelle

```
crates/
├── engine-dsp/   DSP pur, sans I/O, testable sans carte son
│   ├── oscillator.rs   SineOsc : phase f64 normalisée, continue aux changements de fréquence
│   └── smoother.rs     LinearSmoother : rampe dérivée de la cible (pas d'accumulation f32)
└── engine-io/    cpal + thread audio
    ├── processor.rs    SineProcessor : tout ce que le callback possède (indépendant de cpal)
    ├── stream.rs       AudioEngine : ouverture du device, EngineOptions::validate
    ├── diagnostics.rs  Stats (atomiques), XrunDetector (timestamps), DriftMonitor (starvation)
    ├── control.rs      Command (Copy, sans tas) : SetFrequency, SetGain
    └── examples/phase0.rs   driver + test de charge (exit 1 si xrun)
```

Règles non négociables, déjà outillées :

- **Thread audio** : ni allocation, ni lock, ni I/O. C'est vérifié par
  `assert_no_alloc` (en debug) et par le test `tests/no_alloc.rs`.
- **Communication avec le thread audio** : uniquement par ring buffers SPSC
  (`rtrb`), commandes dans un sens, `RtEvent` dans l'autre, plus des compteurs
  atomiques `Stats`.
- **Commandes invalides** (NaN, inf) : rejetées et comptées dans
  `commands_rejected`. Les options invalides sont refusées par
  `EngineOptions::validate`.

## Outils

| Commande | Rôle |
|---|---|
| `scripts/check.sh` | fmt, clippy `-D warnings`, tests debug + release, rustdoc `-D warnings`. `--fast` s'arrête après les lints. |
| `scripts/windows.sh test\|check` | Même chose **sous Windows**, lancé depuis WSL, sur un instantané de l'arbre de travail (y compris le non commité). |
| `scripts/windows.sh phase0 --minutes 30 --gain 0.05` | Run temps réel sur la vraie sortie audio Windows. Log dans `target/windows-logs/`. |
| Hooks git (`.githooks/`) | `pre-commit` → `check.sh --fast` ; `pre-push` → `check.sh`. Activés par `git config core.hooksPath .githooks` (à refaire après un nouveau clone). |
| CI (`.github/workflows/ci.yml`) | `check.sh` sur Linux + Windows. **Inactive : pas encore de remote GitHub.** |

Tests : unitaires dans chaque module, tests de propriétés `proptest` dans
`crates/*/tests/properties.rs`. Les fichiers `*.proptest-regressions` sont
commités, pour que les cas qui ont déjà trouvé des bugs soient rejoués à chaque
exécution.

## Environnement : les pièges déjà rencontrés

- **Pas de validation temps réel sous WSL.** L'audio de WSLg (PulseAudio → RDP)
  décroche systématiquement vers 70-80 s, y compris avec `paplay`, donc
  indépendamment de notre code. Récupération : `wsl --shutdown`. Sous WSL,
  uniquement les tests hors temps réel.
- **`C:\dev\Daw` est un miroir** du dépôt WSL, alimenté par `scripts/windows.sh`.
  N'y rien modifier. Côté Windows, `safe.directory` a été ajouté pour le chemin
  `\\wsl.localhost\Ubuntu\...` (la distro s'appelle `Ubuntu`).
- **Rafales WASAPI toutes les 10 min**, à xx:x0:26 heure murale : buffers de
  960/1056 frames pour rattraper un callback retardé. Inaudibles et sans perte
  à 10 ms de buffer. Cause probable, non vérifiée : G HUB (casque sans fil).
  À surveiller quand les buffers seront plus petits.
- **Le casque PRO X 2 s'expose en 7.1 (8 canaux).** Le sinus est dupliqué sur
  les 8. En phase 1, il faudra mapper les fichiers stéréo vers les canaux
  avant gauche/droite.
- **WASAPI en mode partagé** impose 480 frames (10 ms) : `--buffer` est ignoré.
- **Le plugin ALSA `pulse`** (hôte par défaut sous WSL) inonde le callback
  d'erreurs de `BackendError`, qui sont comptées par type (`error_kinds`).
  Sous WSL, utiliser `--features pulseaudio --host pulseaudio`.
- `cargo` n'est pas toujours dans le PATH des shells non interactifs.
  `check.sh` source `~/.cargo/env` si besoin.

## Phase 1 — plan proposé

Incréments, chacun testé et commité séparément :

1. **Biquad RBJ** passe-bas et passe-haut dans `engine-dsp`. Validation par mesure de la
   réponse (pente -12 dB/oct, gain à la coupure).
2. **Modulation de la coupure sans zipper** : coupure lissée par
   `LinearSmoother`, coefficients recalculés tous les N samples. Tests de
   continuité et de stabilité pendant un balayage.
3. **Crate `engine-io-file`** : décodage avec `symphonia`, et rééchantillonnage
   44,1 → 48 kHz.
4. **Thread de préchargement et transport** (lecture, pause, seek, boucle).
   Un compteur de génération sert à jeter l'audio obsolète après un seek, et
   des micro-fondus évitent les clics sur les pauses et les seeks.
5. **Intégration dans `engine-io`** et exemple `phase1` lancé via
   `scripts/windows.sh`. S'y ajoute un rendu hors ligne du pipeline dans les
   tests, pour mesurer l'absence de discontinuité aux seeks.

### Décisions en attente (à trancher au début de la prochaine session)

1. **Qui écrit le code ?** Claude, ou l'utilisateur guidé par Claude sur les
   parties clés (biquad, protocole de seek). Le projet vise l'apprentissage.
2. **Dépendances** : `symphonia` (décodage) et `rubato` (rééchantillonnage) ?
   Alternative temporaire : n'accepter que des fichiers à 48 kHz.
3. **Validation du biquad** : sonde à sinus, sans dépendance, ou FFT
   (`rustfft` en dev-dependency), comme le prévoit le plan ?

Par défaut, si rien n'est précisé : Claude code l'incrément 1 avec la sonde à
sinus, sans nouvelle dépendance.
