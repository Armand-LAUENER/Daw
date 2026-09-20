# Reprise — état du projet au 2026-09-20

À lire en premier en reprenant le projet. Le plan complet et le journal de bord
sont dans [`plan-daw-rust.md`](../plan-daw-rust.md).

## Où on en est

**Phase 0 terminée et validée** (commit `e33d571`) : sinus temps réel piloté par
un thread de contrôle, 10 min puis 30 min sans xrun en natif Windows (WASAPI),
aucun clic audible aux changements de note.

**Phase 1, incréments 1 et 2 faits** (`engine-dsp` uniquement, rien n'est encore
branché sur le thread audio) :

1. **Biquad RBJ** passe-bas et passe-haut, forme directe II transposée. Validé
   par sonde à sinus, sans dépendance : -3,01 dB à la coupure, -12 dB/octave,
   pic de résonance ≈ Q, blocage du continu.
2. **`SmoothedBiquad`** : coupure lissée et coefficients interpolés, de sorte
   qu'un balayage bouge moins que le signal lui-même.

**Prochaine étape : incrément 3**, la lecture de fichier (`engine-io-file`). La
décision sur les dépendances de décodage reste à prendre (voir en bas).

Le dépôt est sur GitHub (`Armand-LAUENER/Daw`, privé) et la CI y tourne sur
Linux et Windows.

## Architecture actuelle

```
crates/
├── engine-dsp/   DSP pur, sans I/O, testable sans carte son
│   ├── oscillator.rs       SineOsc : phase f64 normalisée, continue aux changements de fréquence
│   ├── smoother.rs         LinearSmoother : rampe dérivée de la cible (pas d'accumulation f32)
│   ├── biquad.rs           Coefficients RBJ (calcul f64, stockage f32) + Biquad en forme directe II transposée
│   ├── smoothed_biquad.rs  SmoothedBiquad : coupure lissée en octaves, coefficients interpolés
│   └── test_util.rs        Banc de mesure des tests : gain par sonde à sinus, plus grand pas de sortie
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
| CI (`.github/workflows/ci.yml`) | `check.sh` sur Linux + Windows, sur le dépôt privé `Armand-LAUENER/Daw`. |

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

1. ~~**Biquad RBJ** passe-bas et passe-haut dans `engine-dsp`.~~ **Fait.**
2. ~~**Modulation de la coupure sans zipper.**~~ **Fait**, mais pas comme prévu :
   voir « Ce que les mesures ont corrigé » ci-dessous.
3. **Crate `engine-io-file`** : décodage avec `symphonia`, et rééchantillonnage
   44,1 → 48 kHz.
4. **Thread de préchargement et transport** (lecture, pause, seek, boucle).
   Un compteur de génération sert à jeter l'audio obsolète après un seek, et
   des micro-fondus évitent les clics sur les pauses et les seeks.
5. **Intégration dans `engine-io`** et exemple `phase1` lancé via
   `scripts/windows.sh`. S'y ajoute un rendu hors ligne du pipeline dans les
   tests, pour mesurer l'absence de discontinuité aux seeks.

### Ce que les mesures ont corrigé

Quatre croyances fausses, chacune démolie par un test et gardée comme test :

1. **« Changer les coefficients garde la sortie continue. »** Faux :
   `y = b0·x + z1`, donc un saut de `b0` fait sauter la sortie de `Δb0·x`. Ce
   qui est vrai, c'est que le saut est *proportionnel* au changement — d'où le
   lissage.
2. **« La sortie reste bornée sous n'importe quelle modulation. »** Faux : un
   filtre calé près du continu garde son transitoire des secondes, et des
   commutations brutales successives pompent jusqu'à +40 dB (mesuré : 105× à
   1 Hz). Seule la **finitude** tient inconditionnellement.
3. **« Lisser la coupure en hertz suffit. »** Faux : de 20 kHz à 20 Hz, une
   rampe linéaire en hertz passe neuf dixièmes du temps au-dessus de 1 kHz puis
   traverse toute la bande audible en une fraction de ms — un saut déguisé.
   `SmoothedBiquad` lisse donc `log2(coupure)`.
4. **« Recalculer les coefficients tous les N samples suffit. »** Faux : 15
   recalculs sur une rampe de 480 samples restent 15 marches. Il faut
   **interpoler** les coefficients entre deux recalculs (5 additions par sample
   contre un sinus). Effet de bord mesuré : l'intervalle de recalcul devient
   presque indifférent, la CPU se règle donc librement.

Deux limites connues, assumées et documentées dans le code :

- **Dénormaux** : après un passage en silence l'état du filtre décroît vers des
  dénormaux, coûteux sur x86 et candidats à un xrun. Rien n'a été ajouté sans
  mesure — à mesurer en branchant le filtre sur `engine-io`.
- **Rampe contre temps d'établissement** : lisser un paramètre n'aide que tant
  que le filtre s'établit plus vite que la rampe ne défile. Une rampe de 10 ms
  vaut `1/(2π·0,01)` ≈ 16 Hz : en dessous de ~20 Hz de coupure, le changement
  dépasse le filtre et ce qu'on mesure est son transitoire propre. Les tests de
  propriétés restent donc au-dessus de 100 Hz.
- **Résonance en balayage** : à Q > 1/√2, un filtre balayé dépasse
  transitoirement son pic en régime établi. C'est physique, pas un clic. Les
  tests de propriétés bornent donc le *pas* de sortie seulement pour Q ≤ 1/√2,
  et la bornitude séparément pour les Q résonants.

### Performance mesurée

Deux mesures, deux questions. En marche : `Stats` tient un histogramme de la
charge par callback, et `phase0` affiche p50 / p99 / max — le pic seul ne dit
rien, un unique dépassement le fixe pour tout le run. Hors ligne :
`scripts/windows.sh bench` (ou `cargo run --release -p engine-dsp --example
bench`) donne le débit en ns/échantillon.

Relevé du 2026-09-20, ns/échantillon :

| cas | WSL | Windows natif |
|---|---|---|
| SineOsc | 9,01 | 4,10 |
| LinearSmoother | 0,47 | 0,42 |
| Biquad | 4,52 | **2,25** |
| SmoothedBiquad, au repos | 4,49 | **4,00** |
| SmoothedBiquad, en balayage | 6,04 | 4,68 |

**Ne jamais conclure sur des chiffres WSL.** Sous WSL, `SmoothedBiquad` au repos
semblait coûter exactement le prix d'un `Biquad` nu, d'où la conclusion — fausse
— que l'arrêt des mises à jour était gratuit. Sous Windows le `Biquad` descend à
2,25 ns et le surcoût réel apparaît : **environ 75 %**, masqué par une baseline
WSL deux fois trop lente.

Cause probable, non encore vérifiée : les branches `stale` et `gliding` peuvent
écrire les coefficients, donc le compilateur les recharge depuis la mémoire à
chaque échantillon au lieu de les garder en registres. Correction attendue : un
`process_block`, qui teste l'état une fois par buffer puis déroule une boucle
serrée — c'est de toute façon la forme qu'appelle le callback.

### Décisions prises

- **Objectif** : une application utilisable et performante. Ce n'est plus un
  projet d'apprentissage, donc on prend la dépendance mûre plutôt que de
  réécrire, et un détour ne se justifie que par de la performance ou de la
  fiabilité mesurable.
- **Décodage** : `symphonia` (formats) et `rubato` (rééchantillonnage
  44,1 → 48 kHz). Écrire son propre rééchantillonneur n'était motivé que par
  l'apprentissage ; n'accepter que du 48 kHz rendrait l'application inutilisable
  sur la moitié d'une discothèque.
- Claude écrit le code, Armand relit.
- Validation du DSP par sonde à sinus, sans `rustfft`.

### Décisions en attente

Aucune ne bloque l'incrément 3. Elles se posent au moment d'écrire le code
concerné : placement des micro-fondus (préchargement ou callback), marge du ring
buffer de préchargement, mapping des canaux vers un périphérique 7.1, et
traitement des dénormaux — à mesurer avant de trancher.
