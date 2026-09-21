# Reprise — état du projet au 2026-09-21

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

**Phase 1 validée le 2026-09-21**, critère de sortie atteint en automatique via
`scripts/windows.sh phase1 --self-test --minutes 10 --seek-every 5` :

```
[600.02s] callbacks 60001 | xruns 0 | overloads 0 | load p50 0,1 % p99 0,1 % max 1,5 %
seeks 119 | underruns 0
discontinuities 0 | worst step 0,013091 | limit 0,018257
PASS: no xrun, no underrun, no discontinuity
```

Le pire pas vaut exactement la pente propre du sinus de 100 Hz (0,013090 en
théorie) : aucun des 119 seeks n'a ajouté quoi que ce soit au signal. Un run de
40 s sur un vrai MP3 passe aussi, mais sans détection de clic — sur de la
musique un coup de caisse claire est une marche légitime, et aucun seuil ne
l'écarte d'un clic.

**Ce que ça ne prouve pas** : la chaîne est propre sur un signal maîtrisé, pas
que ta musique l'est ; et rien après notre buffer — mixeur Windows,
spatialisation du casque — n'est couvert.

**Phase 2 en cours** : `engine-core` a le tampon planaire, les nœuds de base, le
graphe avec ordre topologique, l'échange atomique de graphe (fondu croisé de
10 ms, graphes retirés renvoyés au thread de contrôle pour y être libérés) et
l'automation sample-accurate, le limiteur master et le test de charge.

L'automation passe par un anneau SPSC `rtrb` de `ParamEvent { param, value,
frame }`, où `frame` est absolu sur la timeline du moteur — le thread de
contrôle ne sait pas où un callback commencera. `Graph::process_automated`
découpe le bloc à chaque frame d'événement, donc un changement tombe au sample
près au lieu d'être arrondi à la frontière du callback (5,3 ms à 256 frames et
48 kHz, largement audible sur un élément joué en rythme). Le découpage ne
réinitialise pas les rampes des nœuds : `automating_a_gain_still_ramps` vérifie
que les deux moitiés se rejoignent sans marche, et
`automating_a_graph_does_not_allocate` tient le chemin hors de l'allocateur.

Le **limiteur** (`Limiter`) est à lookahead, parce qu'un limiteur sans lookahead
ne peut pas empêcher un dépassement : quand il connaît le niveau du pic, ce pic
est déjà la sortie, et tout ce qu'il peut faire c'est l'écrêter. Celui-ci retarde
le signal de 2 ms et surveille le retard, donc le gain est déjà descendu quand le
pic arrive. La garantie est exacte, pas approchée : le maximum glissant couvre
tout le retard (fenêtre de `lookahead + 1`), la descente est dimensionnée pour
arriver dans le temps du retard, et le gain ne remonte que si plus rien dans la
fenêtre ne le réclame. `nothing_escapes_the_ceiling` vérifie ça sur un démarrage
à froid en pleine échelle, des pics isolés et une surcharge tenue.

Le maximum glissant est une file monotone décroissante (O(1) amorti par sample)
plutôt qu'un balayage du retard à chaque sample, qui coûterait le lookahead une
fois de plus sur chacun. Elle est vérifiée contre la version naïve dans
`the_sliding_maximum_agrees_with_scanning_the_window`. La détente est un pôle à
100 ms — lente exprès, une détente rapide re-nivelle tout ce qui passe sous le
transitoire, c'est le « pompage » des limiteurs mal réglés.

**Attention à la latence** : le limiteur ajoute `latency_frames()` (2 ms, soit 96
samples à 48 kHz). Tout ce qui doit s'aligner sur cette sortie — métronome, prise
enregistrée, autre chemin du graphe — devra la compenser. Rien ne le fait encore.

### Test de charge (incrément 8)

`cargo run --release -p engine-core --example load` mesure, et
`cargo test -p engine-core --test load` vérifie. Les cas s'empilent, donc
l'écart entre deux lignes est le coût de ce qu'on a ajouté — un total dit
qu'un graphe est lent, une différence dit quelle partie l'est.

Mesuré le 2026-09-21 sur le desktop (WSL2, blocs de 480 frames à 48 kHz) :

| cas | 8 pistes | 16 pistes | 32 pistes |
|---|---|---|---|
| sources dans un bus | 4,6 µs/piste | 4,6 | 4,6 |
| + gain, pan, mute | 7,4 | 7,5 | 7,7 |
| + limiteur master | 7,9 | 7,6 | 7,6 |
| + automation | 7,7 | 7,6 | 7,7 |
| **% d'un buffer de 10 ms** | **0,62 %** | **1,21 %** | **2,46 %** |

Trois choses à en retenir. Le coût par piste est **plat** de 8 à 32 pistes :
le mixage est linéaire, il n'y a pas de quadratique cachée. Le limiteur et
l'automation sont **gratuits** à l'échelle du bruit de mesure — le limiteur est
un seul nœud sur le master, et le découpage de bloc de l'automation coûte
quelques passes de plus sur un graphe qui en fait 480. Le goulot d'étranglement
est la **tranche de voie** : elle ajoute ~3 µs/piste au-dessus de la source, soit
40 % du coût par piste.

Si ça devient un jour un problème, le premier endroit à regarder est
`Gain::process` et `Mute::process`, qui font `buffer.channel_mut(c)[frame]` dans
une boucle par frame — un accès borné recalculé à chaque sample au lieu de
parcourir le canal une fois. Ça n'en est pas un aujourd'hui : 16 pistes, c'est
1,2 % d'un buffer.

**Run de 30 minutes** (16 pistes, chaîne complète, automation) : moyenne entre
119 et 131 µs par bloc, charge 1,19–1,31 %, dérive du premier au dernier
intervalle de dix secondes **-0,0 %**. Rien ne s'accumule. Le rapport est
découpé par tranches de dix secondes exprès : une moyenne sur tout le run
cacherait justement une file que personne ne vide.

**Le « sans glitch » est vérifié automatiquement**, pas à l'oreille. Un glitch,
c'est un saut plus grand que ce que le signal peut produire ; le test dérive
cette borne (pente des sinus + pente maximale d'une rampe de paramètre + chute
maximale du gain du limiteur) et vérifie que la sortie ne la dépasse jamais, en
suivant aussi les frontières de bloc. Les marges observées :

- 8 pistes : pire pas 0,0298 pour une borne de 0,0599 (50 %)
- 16 pistes : pire pas 0,0488 pour une borne de 0,0825 (59 %)
- un paramètre appliqué **sans rampe** : 1,598, soit **27× la borne**

C'est ce dernier chiffre qui rend les deux premiers utiles — une borne que rien
ne peut dépasser ne prouverait rien. Le pic sort à 0,90000004 dans les deux cas,
donc le limiteur tient bien le plafond pendant tout le run.

Tout ça reste hors périphérique : ça établit le coût et la justesse du graphe,
pas que le callback tient sous un vrai ordonnanceur. C'est ce que fait
l'exemple `phase2`.

### Câblage dans `engine-io` (incrément 9)

`scripts/windows.sh phase2 --tracks 8 --seconds 60 --swap-every 7` fait tourner
le graphe sur le périphérique Windows : le callback pilote un `GraphPlayer`, le
thread de contrôle bouge les faders par automation et remplace le routage
pendant que ça joue.

`engine-core` est une **dev-dependency** de `engine-io`, comme `engine-io-file` :
la bibliothèque reste device-only, le câblage vit dans l'exemple.

Le graphe est stéréo quel que soit le périphérique — le panoramique a besoin
d'une paire — et `map_frames` l'amène ensuite vers la disposition réelle (7.1
ici), exactement comme phase1 le fait pour un fichier. Il est construit pour
2048 frames au plus ; un callback plus gros est rendu en plusieurs passes plutôt
que de dimensionner chaque tampon de nœud pour le pire cas.

Le thread de contrôle ne connaît pas l'horloge du thread audio, donc le callback
publie sa position dans un `AtomicU64` et le contrôle programme 30 ms devant. Un
événement en retard n'est pas perdu : il tombe au début du bloc suivant.

Premier run validé le 2026-09-21 (8 pistes, 60 s, 8 échanges de graphe, 8886
mouvements de fader) :

```
graph: 8 tracks, stereo, limiter at 0.9 | latency 96 frames (2.00 ms)
[   60.02s] callbacks 6002 | xruns 0 | overloads 0 | load p50 0,6 % p99 1,3 % max 2,9 %
fader moves 8886 | refused 0 | graph swaps 8
discontinuities 0 | worst step 0,024764 | limit 0,063652
PASS: no xrun, no discontinuity
```

Le pire pas à 39 % de la limite : le détecteur regardait bien de l'audio.

**Critère de sortie de la phase 2, validé le 2026-09-21** — 30 minutes, 16
pistes (alternant 16 et 8 à chaque échange), via
`scripts/windows.sh phase2 --tracks 16 --minutes 30 --swap-every 23` :

```
[ 1800.00s] callbacks 179999 | xruns 0 | overloads 0 | load p50 1,1 % p99 2,6 % max 4,6 %
fader moves 266475 | refused 0 | graph swaps 78
discontinuities 0 | worst step 0,034680 | limit 0,086271
PASS: no xrun, no discontinuity
```

266 475 mouvements de fader, aucun refusé — la file d'automation n'a jamais
débordé. 78 échanges de graphe pendant la lecture, aucune discontinuité. La
charge est la même à la 30ᵉ minute qu'à la première, ce que le run offline de
30 minutes disait déjà mais sans l'ordonnanceur réel.

Ce run **précède le solo** : la tranche de voie n'avait alors que gain, pan et
mute, donc il ne couvre pas tout le critère. Voir ci-dessous.

### Solo (incrément 10)

Le solo est le seul contrôle auquel une piste ne peut pas répondre seule : une
piste n'est jamais « dé-soloée », elle est silencieuse *parce qu'une autre est
en solo*. La résolution est globale, donc il faut quelque chose qui tienne ce
fait pour toutes — c'est `SoloBus`, un compteur de pistes en solo partagé par
tous les `Solo` d'un graphe. Un bus par graphe : un graphe qui en remplace un
autre a le sien, et le thread de contrôle reporte l'état en reconstruisant avec
les mêmes pistes en solo.

C'est un atomique parce qu'un `Graph` traverse vers le thread audio et que tout
ce qu'il contient doit être `Send` ; un seul thread le lit ou l'écrit à la fois,
donc l'ordering le moins cher suffit.

Comme `Mute`, c'est une rampe et pas un court-circuit du traitement : les nœuds
en amont continuent de tourner, donc relâcher un solo ne redémarre pas un filtre
et ne coupe pas une traîne qui décroissait derrière. C'est ce qui rend le solo
sûr à presser et relâcher pendant que ça joue, ce que le plan demandait
explicitement.

Trois pièges traités :

- **Le compteur compte des pistes, pas des appuis.** `set_soloed` ne touche au
  bus que si l'état change vraiment ; sinon un double appui laisserait toutes
  les autres pistes muettes après le relâchement.
- **Une piste retirée rend son compte** (`Drop`), donc un graphe retiré alors
  qu'une de ses pistes était en solo ne peut pas coincer un bus au-dessus de
  zéro.
- **La valeur de départ ne peut être connue qu'au premier bloc.** Le bus n'a
  pas fini de compter quand un nœud est construit — les autres pistes n'existent
  pas encore. Partir de la mauvaise laisserait passer une piste soloée-out
  pendant toute une rampe, d'où le `settled` résolu au premier `process`.

Le solo n'est **pas exclusif** (plusieurs pistes peuvent l'être, elles jouent
toutes) et ne **prime pas sur le mute** : les deux nœuds sont en série, donc
leurs gains se multiplient et une piste que l'utilisateur a muette le reste
quand on la solo — ce que fait une console.

Le test de charge vérifie maintenant qu'un solo a bien été engagé pendant le
run, au même titre que « le mix n'était pas silencieux » : sans ça il passerait
sans jamais avoir exercé le contrôle que le critère nomme.

**Critère de sortie revalidé avec le solo, le 2026-09-21** — 30 minutes, 16
pistes, tranche complète gain/pan/mute/solo :

```
[ 1800.01s] callbacks 180000 | xruns 0 | overloads 0 | load p50 1,3 % p99 2,6 % max 17,3 %
fader moves 355276 | refused 0 | graph swaps 78
discontinuities 0 | worst step 0,034680 | limit 0,089604
PASS: no xrun, no discontinuity
```

355 276 mouvements contre 266 475 au run précédent, soit exactement le rapport
4/3 attendu en passant de trois paramètres à quatre. Pire pas identique
(0,034680) pour une limite montée à 0,089604, la rampe de solo ayant ajouté son
terme : marge à 39 %.

**Le `max 17,3 %` n'est pas une mesure du moteur.** Le run précédent plafonnait
à 4,6 %. p50 et p99 sont inchangés (1,3 % et 2,6 %), donc ni le coût typique ni
la queue proche n'ont bougé — c'est un `fetch_max` sur 180 000 callbacks, donc
le pire tirage d'une distribution à queue lourde. Et pendant ce run, la même
machine compilait le workspace et passait clippy et la suite de tests, parce que
c'est là que j'ai fait le travail sur le plan. Le chiffre mesure cette
contention, pas le graphe. Les valeurs à retenir sont p50 et p99 ; le seuil
d'overload est à 70 % et rien ne l'a approché.

À refaire sur une machine au repos si on veut un `max` qui veuille dire quelque
chose.

### Deux dettes réglées au passage

**`GraphPlayer::process_automated`** existe maintenant, donc l'automation marche
à travers un échange de graphe. Les changements vont au graphe qui prend la
main ; le graphe sortant garde ses réglages, parce que les deux ont été
construits séparément et qu'un identifiant de nœud ne désigne pas la même chose
dans chacun. Il lui reste `SWAP_FADE_MS` à vivre, donc ce qu'il rate est
inaudible.

Un test a trouvé un vrai bug de commande au passage : `process_automated`
appliquait les événements à `self.current` **avant** que `process` ne récupère
le graphe entrant, donc un changement programmé dans le même bloc qu'un échange
atterrissait sur le graphe sortant — puis était fondu au noir avec lui.
`process_automated` récupère maintenant le graphe entrant en premier.

**La latence est rapportée** : `Node::latency_frames()`, `Graph::latency_frames()`
(plus long chemin vers la sortie, pas la somme des chemins parallèles) et
`GraphPlayer::latency_frames()`. L'exemple l'affiche : 96 frames, 2,00 ms.

**Ce qui n'est toujours pas fait** : rapporter n'est pas compenser. Rien n'aligne
un chemin qui contourne un nœud à latence contre un qui le traverse, donc un
graphe avec des routes parallèles de profondeurs différentes les verra dériver.
Il n'y en a pas aujourd'hui — le limiteur est sur la sortie — mais un départ
d'effet en parallèle en créerait une. C'est de la compensation de délai (PDC),
un vrai morceau, à faire quand un routage en aura besoin.

Le dépôt est sur GitHub (`Armand-LAUENER/Daw`, privé) et la CI y tourne sur
Linux et Windows.

## Phase 3 — Séquenceur & MIDI (en cours)

Faits : enveloppe ADSR, événements de note et piano roll, synthé polyphonique
avec scheduling sample-accurate, oscillateurs band-limités, décodage `.mid`, et
l'exemple `phase3` sur le périphérique. Reste le MIDI externe (`midir`).

**L'ADSR est en segments linéaires, pas exponentiels.** L'exponentielle est la
forme analogique, mais un pôle n'atteint jamais zéro : il faut un seuil d'arrêt,
et une voix laissée à 1e-30 est un dénormal, dont ce projet a déjà mesuré le
coût. Le linéaire atterrit exactement, dans exactement le temps demandé, donc
« cette voix est finie » est un fait que le pool peut exploiter. Toute
transition part du niveau réel, donc une note relâchée pendant son attaque ne
saute pas.

**Le synthé ne découpe pas le bloc.** Le plan dit « découpe le buffer aux points
d'arrivée des events », ce que fait l'automation — mais un synthé génère
échantillon par échantillon, donc il consulte sa file en arrivant sur chaque
frame. Pas de découpage, pas de coût pour le reste du graphe. Ça a demandé une
horloge partagée : `Node::set_frame`, distribuée par le `Graph` et par le
`GraphPlayer` aux **deux** graphes pendant un fondu croisé.

**Vol de voix** : même hauteur (réutilise sa voix) → voix libre → voix relâchée
→ la plus ancienne. Le thread audio ne peut pas demander une voix de plus, donc
le choix est entre voler et faire taire, et une note qui disparaît est pire.

**Saw et square sont band-limités (PolyBLEP).** Pris au pied de la lettre, un
saw à 5 kHz met de l'énergie à 3 kHz par repliement, où aucune de ses harmoniques
n'a affaire, et rien en aval ne peut l'en séparer ensuite. Mesuré par
corrélation contre un saw naïf, au moins 6 dB d'écart exigés.

**`midly` sans `rayon`** : les default-features tirent 6 crates transitifs pour
paralléliser un parsing qui prend des millisecondes hors thread audio. Coupées,
`midly` n'a aucune dépendance. Il vit dans `engine-midi`, comme `symphonia` vit
dans `engine-io-file`.

**Le piège du MIDI n'est pas le format, c'est l'horloge.** Un fichier mesure le
temps en ticks dont la durée dépend d'un tempo qu'il peut changer quand il veut.
Lire le premier et l'appliquer partout décale tout ce qui suit, de plus en plus
loin. Les pistes sont donc fusionnées sur une timeline unique d'abord (un format
1 met son tempo dans une piste dédiée), et chaque changement ne vaut que pour ce
qui le suit.

### Validation sur périphérique, 2026-09-21

`scripts/windows.sh phase3 --self-test --bpm 180 --seconds 60` :

```
self test: sixteenths at 180 BPM, one every 4000 frames
graph: 16 voices, Saw, limiter at 0.9 | latency 96 frames (2.00 ms)
[   60.00s] callbacks 6001 | xruns 0 | overloads 0 | load p50 0,1 % p99 0,2 % max 0,8 %
onsets 721 | off their frame 0 | worst 0 frames (0.000 ms)
PASS: no xrun, every note on its frame
```

**Pire écart : 0 frame sur 721 notes.** Pas « inaudible » — exact.

Comment c'est mesuré : le self-test joue des doubles-croches staccato avec
`sustain = 0`, donc le synthé est réellement silencieux entre elles et il existe
un front à mesurer. Un onset est une transition silence → son ; il devrait
tomber à `frame_de_la_note + latence_du_graphe + 1` (la latence du limiteur,
plus le sample que l'enveloppe passe à zéro avant de s'ouvrir). Les onsets étant
périodiques, l'écart est un modulo, donc le callback ne porte aucune table.

Un vrai fichier est joué mais **pas vérifié** : ses notes se chevauchent, il n'y
a pas de silence entre elles pour mesurer un front. Le self-test provoque ce
qu'il mesure, ce qui est la seule façon que le chiffre veuille dire quelque
chose.

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
