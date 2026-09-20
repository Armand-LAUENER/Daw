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

**Incrément 3 fait** : `engine-io-file` décode WAV/FLAC/MP3/MP4-AAC et convertit
vers le taux du périphérique.

**Incrément 4 fait** : thread de préchargement et transport (lecture, pause,
seek, boucle), avec poignée de main de flush et rampes anti-clic.

**Incrément 5 fait** : tout est branché sur le périphérique, avec l'exemple
`phase1` et le mappage des canaux vers un périphérique 7.1.

**Phase 1 terminée côté code, validation entamée.** Un run de 40 s sur WASAPI
(MP3 stéréo, casque PRO X 2 en 7.1, 9 seeks) passe : 0 xrun, 0 underrun,
charge p50 0,1 % / max 0,4 %. Reste le critère complet : 10 minutes, et une
écoute pour confirmer l'absence de clic aux seeks —
`scripts/windows.sh phase1 FICHIER --minutes 10 --seek-every 5`.

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
├── engine-io/    cpal + thread audio
│   ├── processor.rs    StreamProcessor : tout ce que le callback possède (sinus ou rendu externe)
│   ├── stream.rs       AudioEngine : ouverture du device, EngineOptions::validate
│   ├── diagnostics.rs  Stats (atomiques + histogramme de charge), XrunDetector, DriftMonitor
│   ├── control.rs      Command (Copy, sans tas) : SetFrequency, SetGain
│   ├── channels.rs     map_frames : pose les canaux du fichier sur ceux du périphérique
│   ├── examples/phase0.rs   driver sinus + test de charge (exit 1 si xrun)
│   └── examples/phase1.rs   driver fichier : décodage, transport, filtre, seeks
└── engine-io-file/   fichiers et transport
    ├── decode.rs     symphonia + rubato : DecodedAudio, FileError (alloue, hors thread audio)
    └── transport.rs  Preloader (thread dédié) + Playback (temps réel, sans allocation)
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
| `scripts/windows.sh bench` | Débit du DSP sur la cible réelle. |
| `scripts/windows.sh phase1 FICHIER --minutes 10 --seek-every 5` | Lecture de fichier temps réel, seeks répétés. Le chemin est un chemin **Windows**. Sort en code non nul au moindre xrun ou underrun. |
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
- **Le casque PRO X 2 s'expose en 7.1 (8 canaux).** Le sinus de `phase0` est
  dupliqué sur les 8, ce qui est voulu pour un signal de test. Depuis
  l'incrément 5, `map_frames` pose la stéréo sur la paire avant et met les six
  autres à zéro ; `phase1` passe par là.
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
3. ~~**Crate `engine-io-file`** : décodage et rééchantillonnage 44,1 → 48 kHz.~~
   **Fait**, voir ci-dessous.
4. ~~**Thread de préchargement et transport** (lecture, pause, seek, boucle).~~
   **Fait**, voir ci-dessous.
5. ~~**Intégration dans `engine-io`** et exemple `phase1`.~~ **Fait**, voir
   ci-dessous.

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

- **Dénormaux : mesurés, rien à faire.** Le banc contient trois cas à coupure
  30 Hz, dont un où la queue descend réellement dans la zone dénormale — vérifié
  par un compteur, 591 470 sorties dénormales sur 2 000 000, soit 29,6 %. Il
  coûte **+14 % à +23 %** selon le run, contre deux témoins sans dénormaux qui
  s'accordent à 2,23 ns. La pénalité est donc réelle, mais le filtre pèse
  0,011 % d'un buffer : +20 % le porte à 0,014 %, et cent filtres à 1,4 %. Payer
  une comparaison par échantillon sur *tous* les échantillons pour ça serait un
  mauvais échange, et le FTZ matériel demanderait du `unsafe`, interdit par
  `unsafe_code = "forbid"`. Leçon de méthode : sans le compteur, un premier
  relevé donnait « 2,84 contre 2,82 » et concluait à tort qu'il n'y avait rien —
  le bruit de mesure tourne autour de 10 %.
- **Rampe contre temps d'établissement** : lisser un paramètre n'aide que tant
  que le filtre s'établit plus vite que la rampe ne défile. Une rampe de 10 ms
  vaut `1/(2π·0,01)` ≈ 16 Hz : en dessous de ~20 Hz de coupure, le changement
  dépasse le filtre et ce qu'on mesure est son transitoire propre. Les tests de
  propriétés restent donc au-dessus de 100 Hz.
- **Résonance en balayage** : à Q > 1/√2, un filtre balayé dépasse
  transitoirement son pic en régime établi. C'est physique, pas un clic. Les
  tests de propriétés bornent donc le *pas* de sortie seulement pour Q ≤ 1/√2,
  et la bornitude séparément pour les Q résonants.

### Incrément 5 — fait

`AudioEngine::start_with(opts, render)` ouvre le périphérique et joue ce que
produit une closure, avec **exactement les mêmes diagnostics** que le sinus —
détection de xrun, dérive, histogramme de charge — parce que c'est le même
processeur en dessous. `SineProcessor` est donc devenu `StreamProcessor`, avec
un enum `Renderer` : le sinus de la phase 0 d'un côté, un rendu externe de
l'autre. Le chemin sinus est inchangé, y compris ses performances : il écrit
directement dans le format du périphérique, seul le rendu externe passe par un
tampon f32 intermédiaire.

`engine-io` ne dépend **pas** d'`engine-io-file` : la bibliothèque reste
device-only, et seuls l'exemple `phase1` et le test de pipeline tirent la couche
fichier, en dev-dependency.

`map_frames` (dans `channels.rs`) pose les canaux du fichier sur ceux du
périphérique par index — la stéréo atterrit sur la paire avant d'un 7.1, et les
canaux non alimentés sont mis à zéro plutôt que laissés avec ce qui traînait
dans le buffer.

`tests/pipeline.rs` rend la chaîne complète hors ligne — transport, filtre,
mappage — et c'est le critère de sortie de la phase sous forme de test : des
seeks répétés sous une coupure en mouvement ne doivent produire aucune marche
audible.

**Un vrai clic trouvé par ce test** : en sous-alimentation, `Playback::fill`
remplissait la fin du buffer de zéros en comptant sur la rampe de gain pour
adoucir. Mais la rampe met 240 échantillons à descendre alors que les zéros
commencent immédiatement — si l'audio s'arrêtait après 10 échantillons, le gain
valait encore 0,99 et la marche faisait la pleine amplitude (mesuré : 0,147).
`Playback` **tient désormais la dernière trame** et laisse la rampe l'emmener à
zéro.

### Incrément 4 — fait

`transport.rs` fait tourner un thread de préchargement qui alimente le thread
audio par un ring buffer. `start(source, opts)` rend un `TransportHandle`
(`Play`, `Pause`, `Seek`, `SetLoop`, `Stop`) et un `Playback`, dont `fill()` est
le seul point que le callback audio touche — sans allocation, vérifié par
`crates/engine-io-file/tests/no_alloc.rs`.

Le transport lit une **`Source`** (trait) et non un décodeur : `MemorySource`
couvre le clip en mémoire de l'incrément 3, un décodeur en flux se branchera
sans toucher au transport. C'est aussi ce qui rend le protocole de seek testable
de façon déterministe, sans carte son.

**Le seek passe par une poignée de main**, pas par un compteur de génération
comme le prévoyait le plan. Au moment du seek le ring contient déjà l'audio de
l'ancienne position, et seul le consommateur peut le jeter. Le préchargeur lève
donc `flush_request` puis **cesse d'écrire** jusqu'à ce que le consommateur ait
vidé et répondu sur `flush_ack`. Rien de la nouvelle position n'est écrit tant
que de l'audio périmé est en file : les deux ne peuvent pas se mélanger, et un
consommateur qui ne tourne jamais bloque le préchargeur au lieu de corrompre le
flux. La boucle, elle, n'est pas un seek : le préchargeur revient en arrière
pendant qu'il remplit, l'audio reste continu, aucun flush.

Trois bugs trouvés en écrivant les tests, tous gardés comme tests :

- **Le consommateur vidait le ring en pause**, donc la position avançait en
  silence et l'audio en file était mangé. Une fois le fondu terminé il ne
  consomme plus rien. Conséquence assumée : le fondu lui-même consomme, donc
  reprendre repart 5 ms plus loin.
- **Course sur `position`** : les deux moitiés y écrivaient. Le préchargeur
  publie désormais `seek_position`, le consommateur seul écrit `position`.
- **La fin du morceau était comptée comme un underrun.** Ce n'est pas un
  préchargeur en retard, et ça polluait le seul compteur qui signale un vrai
  problème.

Piège de test à connaître : le canal droit des clips de test porte l'indice de
trame, mais il est multiplié par le gain du fondu. Le lire au premier échantillon
après un seek donne 0 — il faut attendre la fin de la rampe.

### Incrément 3 — fait

`engine-io-file` décode et rééchantillonne : `decode_file(path, target_rate)` et
`decode(source, extension, target_rate)` rendent un `DecodedAudio` (f32
entrelacés, `channels`, `sample_rate`). Les erreurs passent par `FileError`, qui
implémente `Error` et `Display` — aucun panic sur un fichier corrompu.

Trois choix à connaître :

- **Le fichier entier tient en mémoire.** Délibéré : le streaming disque
  appartient au thread de préchargement (incrément 4), le faire ici éclaterait
  la logique de transport sur deux crates.
- **Le délai d'encodeur est retiré.** En 0.6 il n'y a plus de `enable_gapless` :
  chaque paquet porte `trim_start` / `trim_end`, appliqués à la main. Sans ça un
  fichier compressé commence par un filet de silence et finit sur une queue
  inventée — et les albums prévus sans blanc claquent entre les pistes.
- **Le taux et le nombre de canaux viennent des buffers décodés**, pas de
  l'entête de piste, qui ne les porte pas toujours.

Tests : `crates/engine-io-file/tests/decode.rs` construit des WAV en mémoire
plutôt que de commiter un binaire, ce qui garde la fréquence, le taux et le
contenu de chaque canal sous contrôle. Ils vérifient le 44,1 → 48 kHz sans
déplacement de hauteur, la séparation des canaux, l'absence de discontinuité aux
frontières de blocs, et le rejet propre d'octets qui ne sont pas de l'audio.

Piège de mesure rencontré : compter les passages par zéro quantifie au cycle
entier, et un sinus démarrant à la phase 0 n'a pas d'échantillon négatif avant
son premier passage — le compte est donc un cycle trop bas. Sur une fenêtre
d'une seconde, aucune tolérance sous 1 Hz n'est tenable, quoi que fasse l'audio.

Dépendances retenues, `default-features = false` au niveau workspace pour que
chaque crate nomme ses formats au lieu d'embarquer tous les décodeurs :

- `symphonia 0.6.1`, features `wav`, `pcm`, `flac`, `mp3`, `isomp4`, `aac`.
  Attention : `wav` n'est que le conteneur, sans `pcm` les échantillons qu'il
  contient n'ont pas de décodeur. Même découpage pour `isomp4` / `aac`.
- `rubato 5.0.0`.

**Les deux API ont été fortement remaniées** par rapport aux versions
couramment documentées en ligne. Notes relevées dans les sources, à reprendre
telles quelles plutôt que de coder de mémoire :

- `symphonia 0.6` : `FormatReader` porte une lifetime (`Box<dyn FormatReader + 's>`) ;
  `next_packet()` renvoie `Result<Option<Packet>>` ; les codecs audio ont leur
  propre registre (`make_audio_decoder`, `AudioDecoderOptions`) ; les buffers
  passent par `GenericAudioBufferRef`, avec `spec()`, `frames()` et
  `copy_to_slice_interleaved` / `copy_to_vec_interleaved` pour sortir du f32
  entrelacé. `Track::codec_params` est un `Option<CodecParameters>`.
  Point d'entrée : `symphonia::default::get_probe().probe(&hint, mss, fmt_opts, meta_opts)`.
- `rubato 5.0` : plus de `FftFixedIn`. Les types sont `Fft`, `FixedSync`,
  `Async`, `FixedAsync` ; les buffers passent par le crate `audioadapter`. Le
  trait `Resampler` expose `process_all_into_buffer`, `input_frames_next`,
  `output_frames_max`.

Pour les tests, prévoir de construire un WAV en mémoire (sinus à 44,1 kHz) et de
le passer par un `Cursor<Vec<u8>>` : ça évite un fichier binaire dans le dépôt et
permet de vérifier que la fréquence survit au rééchantillonnage vers 48 kHz.

### Performance mesurée

Deux mesures, deux questions. En marche : `Stats` tient un histogramme de la
charge par callback, et `phase0` affiche p50 / p99 / max — le pic seul ne dit
rien, un unique dépassement le fixe pour tout le run. Hors ligne :
`scripts/windows.sh bench` (ou `cargo run --release -p engine-dsp --example
bench`) donne le débit en ns/échantillon.

Relevé du 2026-09-20 sous Windows natif, ns/échantillon (les chiffres par bloc
sont nets du remplissage de buffer, que le banc affiche à part) :

| cas | par échantillon | par bloc |
|---|---|---|
| SineOsc | 4,52 | — |
| LinearSmoother | 0,41 | — |
| Biquad | 2,25 | 2,44 |
| SmoothedBiquad, au repos | 3,98 | **2,32** |
| SmoothedBiquad, en balayage | 4,64 | 3,44 |

**Utiliser `process_block`, pas `process_sample` en boucle.** Appelé échantillon
par échantillon, `SmoothedBiquad` coûte 75 % de plus qu'un `Biquad` nu : ses
branches `stale` et `gliding` peuvent écrire les coefficients, que le compilateur
recharge donc depuis la mémoire à chaque tour. Par blocs il teste l'état une fois
par buffer, et l'enveloppe redevient gratuite — 2,39 contre 2,51 pour le biquad
nu dans le même harnais, soit du bruit.

Deux pièges de méthode, payés une fois chacun :

- **Ne jamais conclure sur des chiffres WSL.** Sous WSL, `SmoothedBiquad` au
  repos semblait coûter exactement le prix d'un `Biquad` nu (4,49 contre 4,52),
  d'où la conclusion — fausse — que l'arrêt des mises à jour était gratuit. La
  baseline WSL était deux fois trop lente et masquait le surcoût.
- **Ne comparer qu'à l'intérieur d'un même harnais.** Le cas par échantillon
  entretient une dépendance de données artificielle pour empêcher l'optimiseur
  de sortir la boucle ; le cas par bloc lit un buffer rempli. Les deux colonnes
  ci-dessus ne mesurent donc pas le même travail et ne se comparent pas ligne
  à ligne.

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
