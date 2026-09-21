# Plan de projet — DAW en Rust

**Objectif du projet** : une station audio utilisable et performante. Chaque phase a un critère de sortie mesurable — ne pas passer à la phase suivante tant que le critère n'est pas vérifié objectivement (mesure, pas impression).

Conséquences sur la façon de trancher : prendre la dépendance mûre plutôt que réécrire, garder la solution la plus rapide à latence égale, et ne faire un détour que s'il achète de la performance ou de la fiabilité mesurable.

**Principe architectural non négociable** : le thread audio (callback `cpal`) ne fait jamais d'allocation, de lock, d'I/O disque/réseau, ni d'attente bloquante. Toute communication avec ce thread passe par des structures lock-free. C'est la règle centrale de tout le projet.

---

## Structure du projet (workspace Cargo)

```
daw/
├── crates/
│   ├── engine-core/      # graphe audio, nodes, scheduling sample-accurate
│   ├── engine-io/        # cpal wrapper, gestion devices, callback
│   ├── engine-dsp/       # filtres, effets, oscillateurs
│   ├── engine-io-file/   # décodage via symphonia, streaming disque
│   ├── engine-midi/      # I/O MIDI (midir), parsing (midly)
│   ├── project-model/    # état projet, sérialisation, undo/redo
│   ├── ui/               # egui/eframe
│   └── plugin-host/      # phase 7, séparée
├── tests/
│   ├── dsp_validation/   # tests spectraux (rustfft)
│   └── stress/           # charge CPU, nombre de pistes, durée
└── docs/
    └── architecture.md   # décisions d'architecture, journal de bord
```

Séparer `engine-core` de `ui` dès le départ, même en phase 0 — ça force la discipline de ne jamais coupler l'état audio à l'état UI, et ça évite une refonte douloureuse en phase 4.

---

## Phase 0 — Fondations du temps réel
**Durée estimée** : 2-4 semaines
**But** : comprendre la contrainte réelle du callback audio avant d'ajouter la moindre complexité métier.

### Tâches
- [x] Setup `cpal` : énumérer les devices, ouvrir un stream de sortie stéréo, callback vide qui écrit du silence.
- [x] Générer un sinus dans le callback (état minimal : phase courante, incrémentée à chaque sample).
- [x] Ajouter un canal de contrôle : `rtrb::RingBuffer` (SPSC) pour envoyer une nouvelle fréquence depuis un thread "contrôle" séparé (simulateur simple, pas encore d'UI).
- [x] Mesurer et logger les xruns/underruns exposés par `cpal` (ou détectés manuellement via timestamp du callback).
- [x] Écrire un test de charge : stream ouvert 30 minutes, changement de fréquence toutes les secondes, aucun underrun.

### Points d'attention
- Le sample rate et la taille de buffer ne sont pas garantis stables sur tous les OS — gère le cas où `cpal` te donne un buffer de taille variable d'un callback à l'autre.
- Ne fais **aucun** `println!` dans le callback (I/O bloquante) — logge via un ring buffer séparé lu par un thread dédié si tu veux du diagnostic en direct.

### Critère de sortie
Stream stable 10+ minutes sans underrun, changement de paramètre en direct sans clic ni glitch audible.

**✅ Validé le 2026-09-19** en natif Windows (WASAPI) : 10 min puis 30 min sans xrun, aucun clic audible aux changements de note (voir journal).

### Ressources
- Documentation `cpal` (exemples du repo GitHub `RustAudio/cpal`)
- Article de référence : "Real-time audio programming 101" (Ross Bencina) — les principes s'appliquent tels quels en Rust.

---

## Phase 1 — Lecture mono-piste avec effet
**Durée estimée** : 1-2 mois
**But** : lire un vrai fichier audio avec transport complet, et faire vivre ton premier nœud DSP dans le flux temps réel.

### Tâches
- [x] Intégrer `symphonia` pour décoder mp3/flac/wav en PCM.
- [x] Thread de préchargement disque : lit des chunks à l'avance dans un ring buffer plus grand (quelques secondes de marge), jamais dans le callback audio.
- [x] Transport : play / pause / seek / loop. Le seek doit vider proprement le ring buffer de préchargement et redémarrer la lecture au bon offset sans glitch.
- [x] Implémenter un filtre biquad résonant (RBJ cookbook — low-pass, high-pass) comme struct qui expose `process_sample(&mut self, x: f32) -> f32`.
- [x] Interpolation des coefficients du filtre entre deux mises à jour de paramètre (éviter le zipper noise) — recalcul progressif sur N samples, pas un switch brutal.
- [x] Test spectral : sweep sinusoïdal en entrée, FFT en sortie, vérifier la pente de coupure attendue (ex. -12dB/octave pour un biquad simple).

### Points d'attention
- Le seek pendant lecture est un point classique de bug : si tu ne resynchronises pas le thread de préchargement avec la nouvelle position, tu lis de l'audio obsolète pendant quelques centaines de ms.
- VBR (bitrate variable) complique le mapping temps → position dans le fichier compressé — `symphonia` gère ça, mais comprends comment avant de debugger un seek qui dérive.

### Critère de sortie
Lecture d'un fichier de 10+ minutes avec seeks répétés sans clic audible ; filtre modulable en direct sans zipper noise, validé par mesure FFT et pas seulement à l'oreille.

### État
Les six tâches sont faites, et le critère de sortie a été validé sur le périphérique le 2026-09-21 (10 min, 119 seeks, aucun xrun ni discontinuité). Le log et les marges sont dans [`docs/REPRISE.md`](docs/REPRISE.md).

Un écart au plan : **la réponse en fréquence est mesurée par sonde à sinus, pas par FFT.** Le filtre est attaqué par un sinus, on laisse le transitoire s'éteindre, puis on compare les RMS d'entrée et de sortie sur la même fenêtre — mesurer l'entrée plutôt que la supposer annule l'essentiel de l'erreur d'un nombre de cycles non entier. Ça mesure exactement ce qu'une FFT mesurerait, aux fréquences qui comptent, sans ajouter de dépendance. Les assertions sont les mêmes qu'au plan : -3,01 dB à la coupure, -12 dB/octave, pic de résonance ≈ Q, blocage du continu, plus un test de propriété qui balaie les coupures de 200 Hz à 8 kHz.

---

## Phase 2 — Graphe audio multi-piste
**Durée estimée** : 2-3 mois
**But** : passer d'un flux linéaire à un vrai graphe de traitement, avec plusieurs pistes qui se mixent.

### Tâches
- [x] Définir le trait `Node` : `fn process(&mut self, input: &mut AudioBuffer)`.
- [x] Implémenter le tri topologique du graphe (pistes → bus → master) — recalculé à chaque modification de routing, jamais dans le callback.
- [x] Nodes de base : gain, pan (loi de panoramique constante en puissance, pas linéaire), mute, solo (le solo doit couper les autres pistes sans qu'elles cessent de tourner en arrière-plan si tu veux un solo "safe" sans glitch au relâchement).
- [x] Automation : rampes de paramètres interpolées sample-accurate, écrites depuis un thread externe via message lock-free consommé au début de chaque callback.
- [x] Bus master avec limiteur simple en sortie (protection contre le clipping numérique pendant les tests).
- [x] Test de charge : 8, puis 16 pistes simultanées, mesure du CPU par piste, identification du goulot d'étranglement (allocation cachée, copie de buffer superflue).

### Points d'attention
- Le tri topologique doit être recalculé hors du thread audio et livré au thread audio via un swap atomique de graphe (double-buffering), jamais modifié en place pendant qu'il tourne.
- Attention aux copies de buffer implicites (`Vec::clone` caché dans un `.collect()`) — chaque copie inutile dans le chemin audio est un risque de latence.

### Critère de sortie
8 pistes mixées simultanément, paramètres (gain/pan/mute/solo) modifiables en live sans glitch, usage CPU mesuré et stable sur 30 minutes de lecture continue.

### État
Les six tâches sont faites. Mesures, marges et runs de validation dans [`docs/REPRISE.md`](docs/REPRISE.md) ; le détail ne vit pas ici.

Deux écarts au plan valent d'être notés :

- **L'automation fait mieux que demandé.** Le plan disait « consommé au début de chaque callback » ; les événements portent une frame absolue et le bloc est découpé à chacune, donc un changement tombe au sample près. À 256 frames et 48 kHz, la frontière du callback est à 5,3 ms — audible sur un élément joué en rythme.
- **La latence est rapportée, pas compensée.** `Graph::latency_frames()` donne le plus long chemin vers la sortie (96 frames avec le limiteur). Rien n'aligne un chemin qui contourne un nœud à latence contre un qui le traverse. Il n'y a pas de route parallèle aujourd'hui, mais un départ d'effet en créerait une : la vraie compensation de délai (PDC) reste à faire, à ranger dans la phase qui en aura besoin.

---

## Phase 3 — Séquenceur & MIDI
**Durée estimée** : 1-2 mois
**But** : ajouter un synthé interne piloté par MIDI, avec scheduling sample-accurate.

### Tâches
- [x] I/O MIDI externe via `midir` (réception de notes depuis un contrôleur/clavier).
- [x] Parsing de fichiers `.mid` via `midly` pour rejouer une séquence enregistrée.
- [x] Synthé simple : oscillateur (sine/saw/square) + enveloppe ADSR par voix, polyphonie basique (pool de voix réutilisables, pas d'allocation à chaque note).
- [x] Scheduling intra-buffer : une note qui démarre au milieu d'un buffer de 512 samples doit démarrer exactement au bon sample, pas au début du buffer suivant (sinon dérive audible sur un tempo rapide).
- [x] Piano roll minimal en mémoire (liste de notes avec timestamp/durée/pitch/vélocité) pour tester le scheduling sans dépendre du matériel MIDI.

### Points d'attention
- Le piège classique : traiter tout le buffer d'un coup puis "appliquer" les events MIDI au début du buffer suivant. Ça introduit jusqu'à ~10ms de latence perçable au tempo élevé. Découpe le buffer aux points d'arrivée des events si tu veux du vrai sample-accurate.

### Critère de sortie
Lecture d'un fichier MIDI sans décalage audible même à tempo élevé (160+ BPM), réaction en direct à un contrôleur externe avec latence perçue nulle.

**Première moitié atteinte, seconde non validée.** Les cinq tâches sont faites ; le critère, lui, ne l'est qu'à moitié.

| moitié | état |
|---|---|
| Fichier MIDI sans décalage à 160+ BPM | **atteint** — 30 min à 180 BPM sur le périphérique, 21 601 notes, pire écart **0 frame** |
| Contrôleur externe, latence perçue nulle | **non validé** — aucun clavier MIDI disponible |

### État
Mesures et runs dans [`docs/REPRISE.md`](docs/REPRISE.md) ; le détail ne vit pas ici.

Trois écarts au plan valent d'être notés :

- **Le synthé ne découpe pas le buffer.** Le plan dit « découpe le buffer aux points d'arrivée des events », ce que fait l'automation de la phase 2 — mais un synthé génère échantillon par échantillon, donc il consulte sa file en arrivant sur chaque frame. Même précision, aucun coût pour le reste du graphe. Ça a demandé une horloge partagée (`Node::set_frame`), distribuée par le graphe et aux deux graphes pendant un fondu croisé.
- **Saw et square sont band-limités (PolyBLEP).** Le plan ne le demandait pas. Pris au pied de la lettre, un saw à 5 kHz met de l'énergie à 3 kHz par repliement, où aucune de ses harmoniques n'a affaire, et rien en aval ne peut l'en séparer ensuite.
- **`midir` est derrière une feature `live`.** Lire un `.mid` ne doit pas lier la pile MIDI système : sans la feature `engine-midi` coûte 5 crates, avec elle 13. Le code du clavier est écrit et sa traduction de messages testée, mais l'ouverture d'un port réel n'a jamais été exercée — et les ports virtuels ne sont pas une échappatoire (`midir` n'en a pas sur Windows, WSL n'expose pas `/dev/snd/seq`). `scripts/windows.sh phase3 --midi-in` est prêt pour le jour où un clavier sera branché.

---

## Phase 4 — Interface graphique
**Durée estimée** : 2-3 mois (parallélisable avec la phase 5)
**But** : rendre le projet utilisable sans terminal, sans jamais coupler l'UI à l'état audio directement.

### Tâches
- [ ] Setup `egui` + `eframe`, boucle de rendu à 60fps découplée du thread audio.
- [ ] Vue transport (play/pause/stop/position) qui lit l'état audio via un canal de lecture seule (ex. `arc-swap` sur un snapshot d'état), jamais un accès direct partagé.
- [ ] Mixer : faders de gain, pan, mute/solo par piste, écriture des changements via le même canal de contrôle lock-free que la phase 2.
- [ ] Rendu de waveform : pré-calcule des min/max par bloc de N samples (peak file), ne recharge jamais le fichier entier pour l'affichage.
- [ ] Timeline avec placement de clips (drag & drop basique).
- [ ] Undo/redo : pile de commandes réversibles sur le `project-model` (état projet), complètement séparée de l'état temps réel du moteur audio.

### Points d'attention
- Ne mélange jamais "état projet" (ce qui est sauvegardé, undoable) et "état moteur audio temps réel" (ce qui tourne dans le callback) — ce sont deux sources de vérité différentes qui se synchronisent via messages, pas une seule structure partagée.

### Critère de sortie
UI fluide à 60fps avec 8 pistes en lecture, chaque action a un undo qui restaure l'état exact précédent, aucune interaction UI ne cause de glitch audio.

**Critère revu le 2026-09-22.** « Fluide à 60 fps » est faible : de l'immediate mode avec quelques widgets y arrive sans effort, et ce n'est pas ce qui casse. Le vrai risque est que le thread UI perturbe le thread audio *indirectement* — par le mécanisme d'instantané, une allocation sur un chemin partagé, une contention. Remplacé par :

- **pendant une action UI continue** (fader tiré sans relâche, timeline scrollée en pleine lecture, pistes ajoutées et retirées) : **zéro xrun et zéro discontinuité sur 10 minutes**, mesurés par les compteurs du moteur et le détecteur à borne analytique de la phase 2 ;
- **undo** : test de propriété sur des séquences de commandes arbitraires, l'historique remonté pas à pas contre un instantané de chaque état ;
- **fluidité** : percentiles de temps de frame, sur le modèle de l'histogramme de charge.

Les trois sont mesurables. Voir `docs/REPRISE.md` pour l'état : le premier est partiellement établi (0 xrun sur ~12 min d'interaction), le détecteur de discontinuité n'est pas encore branché sur l'UI.

---

## Phase 5 — Effets intégrés
**Durée estimée** : 1-2 mois
**But** : enrichir la boîte à outils DSP avec des effets standards, testés rigoureusement.

### Tâches
- [ ] EQ paramétrique multi-bande (extension du biquad de la phase 1).
- [ ] Compresseur (détection d'enveloppe RMS ou peak, ratio/threshold/attack/release).
- [ ] Reverb algorithmique (Schroeder ou Freeverb — comb filters + all-pass en série).
- [ ] Delay synchronisé au tempo (conversion BPM → samples de délai, avec feedback contrôlé).
- [ ] Suite de tests DSP systématique : sweep sinusoïdal + FFT pour chaque effet, comparaison à la réponse théorique attendue.

### Points d'attention
- Ne valide jamais un effet "à l'oreille" seul — un compresseur qui semble bien sonner peut avoir un bug de détection d'enveloppe qui ne se voit qu'en traçant la courbe de gain réduction dans le temps.

### Critère de sortie
Chaque effet a une réponse mesurée (fréquentielle ou temporelle selon le type) conforme à la théorie, pas seulement une validation subjective.

---

## Phase 6 — Persistance de projet
**Durée estimée** : 2-4 semaines
**But** : sauvegarder et recharger un projet complet sans perte de données.

### Tâches
- [ ] Sérialisation `serde` de l'état `project-model` complet (pistes, clips, automation, routing).
- [ ] Stratégie de versioning du format dès le premier commit (numéro de version dans le fichier, migration explicite si le format change).
- [ ] Test de round-trip : sauvegarde → rechargement → comparaison bit-à-bit de l'état projet.
- [ ] Gestion des chemins de fichiers audio relatifs vs absolus (portabilité d'un projet entre machines).

### Critère de sortie
Sauvegarde/rechargement d'un projet complexe (16 pistes, automation, effets) sans perte de données, y compris après un changement mineur de version du format (migration testée).

---

## Phase 7 — Hébergement de plugins VST3/CLAP (optionnelle, séparée)
**Durée estimée** : 3-6+ mois
**À ne démarrer que si les phases 1-3 sont stables sous charge réelle, pas juste sur ton exemple de test.**

### Pourquoi c'est à part
Pour une station réellement utilisable, l'hébergement de plugins n'est pas accessoire : c'est ce qui décide si l'application sert à travailler ou reste une démo. Mais c'est aussi le poste le plus coûteux et le plus risqué du projet — plomberie FFI vers un SDK C++, fenêtre GUI d'un process tiers, isolation de crash — et il ne rend rien tant que le moteur n'est pas solide. D'où l'ordre : phases 1-3 d'abord, sous charge réelle, puis celle-ci.

### Tâches
- [ ] Étudier `vst3-sys` (binding FFI vers le SDK C++ Steinberg) — comprendre le modèle de composants VST3 (edit controller, processor, IPlugView).
- [ ] Alternative plus propre : `clap-sys` (ABI en C, plus simple à binder) — écosystème de plugins existants plus restreint.
- [ ] Isolation de crash : un plugin tiers qui segfault ne doit pas planter l'hôte (sandbox par process séparé, communication IPC — sujet à lui seul).
- [ ] Embedding de la fenêtre GUI du plugin dans ta propre UI `egui` (gestion de handle de fenêtre natif, différent par OS).

### Critère de sortie
Chargement d'au moins un plugin VST3 ou CLAP réel, traitement audio à travers ce plugin sans glitch, crash du plugin isolé sans faire planter l'hôte.

---

## Vue d'ensemble et estimation

| Phase | Durée estimée | Cumulé |
|---|---|---|
| 0 — Fondations temps réel | 2-4 semaines | 1 mois |
| 1 — Lecture mono-piste | 1-2 mois | ~3 mois |
| 2 — Graphe multi-piste | 2-3 mois | ~6 mois |
| 3 — Séquenceur & MIDI | 1-2 mois | ~7,5 mois |
| 4 — Interface graphique | 2-3 mois (parallèle) | ~9,5 mois |
| 5 — Effets intégrés | 1-2 mois | ~11 mois |
| 6 — Persistance | 2-4 semaines | ~12 mois |
| **Total phases 0-6** | | **~9 à 14 mois** |
| 7 — Plugin hosting (optionnel) | 3-6+ mois | +3 à 6 mois |

**Estimation réaliste en temps partiel** (soirs/weekends) : applique une marge de type Hofstadter — compte **1,5 à 2 ans** pour un moteur stable phases 0-6, sans plugin hosting. Ce n'est pas pessimiste, c'est la norme documentée pour ce type de projet solo hors temps plein.

---

## Ordre révisé (2026-09-22)

Les phases 0 à 3 sont faites et validées sur matériel ; la 4 est entamée et utilisable. L'ordre du plan d'origine tenait pour un projet qu'on n'avait pas encore commencé. Ce qu'on sait maintenant le change sur deux points.

**La persistance passe devant la timeline.** La phase 6 est la plus petite qui reste et c'est celle qui débloque le reste : aujourd'hui chaque relance perd les fichiers chargés et tous les réglages, donc chaque cycle de test des phases suivantes recommence à zéro. Construire la timeline avant la sauvegarde voudrait dire replacer les clips à la main à chaque essai. Le modèle est prêt — données simples, champ `version` depuis le premier commit, chemin de fichier porté par la piste.

**Le transport passe devant les effets.** Il n'y a aucun moyen d'arrêter le son : un fichier chargé boucle indéfiniment. Play/pause/stop et une position lue par l'UI sont petits, et une timeline ne veut rien dire sans eux.

| ordre | ce que c'est | phase d'origine | taille |
|---|---|---|---|
| 1 | Sauvegarde et chargement de projet | 6 | petite |
| 2 | Transport (play/pause/stop, position) | 4 | petite |
| 3 | EQ paramétrique | 5 | moyenne |
| 4 | Waveform (peak files) et timeline avec clips | 4 | **la plus grosse du projet** |
| 5 | Compresseur, reverb, delay | 5 | moyenne |
| 6 | MIDI externe validé sur matériel | 3 | en attente d'un clavier |
| 7 | Hébergement CLAP | 7 | optionnelle |

Chaque étape ajoute sa fonctionnalité à l'UI au passage, plutôt que d'attendre une phase « interface » séparée : c'est l'usage qui trouve les défauts, et trois l'ont déjà prouvé (voir le journal).

---

## Ce que le projet a appris

**Le CPU n'a jamais été la contrainte.** Le plan consacre plusieurs points d'attention au coût par piste. Mesuré : 16 pistes avec tranche complète et automation coûtent **1,2 % d'un buffer de 10 ms**, deux fichiers stéréo mixés **0,1 %**. Il y a un ordre de grandeur de marge. Le goulot identifié — la tranche de voie, ~3 µs/piste — ne vaut pas la peine d'être touché.

**Les défauts réels portaient sur l'état, pas sur la vitesse.** Aucun des bugs sérieux de ce projet n'était du DSP : un `RefCell` emprunté pendant une reconstruction de graphe, un undo qui enregistrait soixante pas pour un geste, une énumération de source qui jetait le chemin du fichier, un événement d'automation appliqué au graphe sortant au lieu de l'entrant. Tous sont des désaccords entre deux dépositaires de la même vérité. C'est là qu'il faut regarder en priorité, pas sur les nanosecondes.

**« Mesure, pas impression » tient aussi pour l'interface.** On croyait ce critère réservé au moteur. Deux des trois moitiés du critère de la phase 4 se mesurent avec ce qui existe déjà, et la troisième aussi. Un critère qui ne se mesure pas est un critère à réécrire, pas une exception à accepter.

**Les surfaces d'extension sont décidées et elles sont petites.** `Node` pour le DSP (déjà stable : `process`, `reset`, `set_param`, `set_frame`, `latency_frames`), `Command` pour tout ce qui s'édite et s'annule, le thème en donnée, et CLAP en phase 7 pour les tiers. Rust n'ayant pas d'ABI stable, un système de plugins maison contaminerait tout le code en `#[repr(C)]` pour servir zéro contributeur aujourd'hui ; CLAP résout déjà le problème.

**La règle sur les dépendances a payé.** `midly` sans `rayon` (6 crates évités pour un parsing de quelques millisecondes), pas de `rfd` (le glisser-déposer d'egui suffit et fait mieux), pas de `serde` tant qu'aucun fichier n'est écrit. Chaque ajout a été justifié et pesé.

---

## Journal de bord (à tenir à jour)

Section à compléter au fil du projet : décisions d'architecture prises, bugs non triviaux résolus (et pourquoi ils arrivaient), benchmarks CPU par phase. C'est ce journal qui devient la vraie preuve de compétence si tu veux en parler en entretien — pas le code final seul.

| Date | Phase | Décision / bug résolu | Pourquoi |
|---|---|---|---|
| 2026-09-19 | 0 | Passage en workspace Cargo (`engine-dsp`, `engine-io`) dès la phase 0. | Force la séparation DSP pur / I/O ; `engine-dsp` se teste sans carte son. |
| 2026-09-19 | 0 | Logique du callback isolée dans `SineProcessor`, indépendante de cpal ; callback enveloppé dans `assert_no_alloc` + test `no_alloc`. | Rend la règle « zéro allocation dans le thread audio » vérifiable automatiquement, pas par relecture. |
| 2026-09-19 | 0 | Xruns détectés par deux voies : `ErrorKind::Xrun` de cpal 0.18 et saut de timestamp de playback expliqué par un callback en retard (sinon `LatencyShift`). | Le plugin ALSA `pulse` révise son estimation de latence au démarrage : un saut de playback seul donnait de faux xruns. |
| 2026-09-19 | 0 | Error callback : compteurs atomiques par `ErrorKind`, sans formatage ni allocation. | Un compteur global cachait 3,9 M de `BackendError` (worker ALSA de cpal en boucle sur une erreur du plugin `pulse`). |
| 2026-09-19 | 0 | **Bug environnement** : sous WSLg, le stream se dégrade systématiquement vers 70-80 s (ALSA 512/1365 frames, host PulseAudio défaut/1024). Callback < 3 % de charge mais appelé de moins en moins souvent. | Point commun unique : le sink RDP de WSLg. Validation temps réel de la phase 0 à faire en natif Windows (WASAPI). |
| 2026-09-19 | 0 | **Cause WSLg confirmée par test témoin** : `paplay` d'un WAV de 180 s (même signal, s16le 44,1 kHz, sans notre code ni cpal) se dégrade aussi vers 70 s ; lecture non terminée après 7 min 38 s ; serveur PulseAudio de WSLg ne répond plus (`pactl` en timeout) à partir de ~150 s. Latence du sink RDP stable à ~1,1 s après 50 s. | Ce n'est ni notre code ni cpal. Ce n'est pas non plus un tampon qui sature par dérive d'horloge (latence stable) : le serveur ou son lien RDP décroche puis se bloque. Récupération : `wsl --shutdown`. Tests temps réel uniquement en natif Windows ; WSL réservé aux tests hors temps réel. |
| 2026-09-19 | 0 | Détecteur de starvation `DriftMonitor` : avance cumulée (frames livrées − temps écoulé × SR) comparée à une référence à fuite de 1000 ppm. | Le host PulseAudio fige les timestamps de playback pendant un décrochage : le détecteur par timestamps classait le ralentissement progressif en `LatencyShift`. Validé sur WSLg : 0 événement sur 0-55 s, starvation détectée dès 55,8 s. |
| 2026-09-19 | 0 | Tests de propriétés (`proptest`) sur le DSP, les détecteurs et le processeur ; `scripts/check.sh` (fmt, clippy `-D warnings`, tests debug + release, doc) branché sur des hooks git et une CI Linux + Windows. | Les tests unitaires ne couvrent que des valeurs choisies à la main. proptest a trouvé dès le premier run un défaut que ces tests ne voyaient pas. |
| 2026-09-19 | 0 | `LinearSmoother` calcule chaque valeur depuis la cible (`target - step × remaining`) au lieu d'accumuler `current += step`. | L'accumulation en f32 dérivait jusqu'à dépasser la cible (≈ 0,009 Hz à 4,4 kHz ; un gain à 1,0 pouvait dépasser 1,0) avant le recalage final, ce qui cassait la monotonie de la rampe. |
| 2026-09-19 | 0 | **Phase 0 validée en natif Windows (WASAPI, casque PRO X 2 en 7.1, 48 kHz, buffer imposé à 480 frames = 10 ms).** 10 min puis 30 min : 0 xrun, 0 erreur de flux, 1800/1800 commandes appliquées, 86 404 032 frames en 1800,1 s (48 000/s, pas de dérive), charge max 1,4-6,1 %. Aucun clic aux changements de note ni pendant les rafales ci-dessous (écoute). | Confirme que les décrochages de WSL venaient de WSLg, pas de l'architecture. Tests temps réel désormais lancés depuis WSL par `scripts/windows.sh phase0` (logs dans `target/windows-logs/`). |
| 2026-09-19 | 0 | Rafales de rattrapage WASAPI (buffers de 960/1056 frames, parfois `LatencyShift` jusqu'à +820 frames) **toutes les 10 min pile, à xx:x0:26 heure murale**, prédites puis vérifiées (1591,18 s). Sans perte : le débit total reste exact, et aucun détecteur ne se déclenche. | Tâche périodique de Windows qui retarde un callback (hypothèse non vérifiée : G HUB interroge le casque sans fil). Inaudible à 10 ms de buffer, mais à surveiller quand les buffers seront plus petits (phase 2+). |
| 2026-09-20 | 1 | Réponse en fréquence mesurée par **sonde à sinus** (rapport RMS entrée/sortie après extinction du transitoire) plutôt que par FFT comme le plan le demandait. | Mesure exactement la même chose aux fréquences qui comptent, sans ajouter `rustfft`. Mesurer l'entrée plutôt que la supposer à 1/√2 annule l'essentiel de l'erreur d'un nombre de cycles non entier. |
| 2026-09-21 | 2 | Automation par événements portant une **frame absolue**, et bloc découpé à chaque frame d'événement (`EventQueue::span`). | Le plan disait « consommé au début de chaque callback ». À 256 frames et 48 kHz, la frontière du callback est à 5,3 ms — audible sur un élément joué en rythme. |
| 2026-09-21 | 2 | Limiteur master **à lookahead** (2 ms), garantie exacte : maximum glissant sur tout le retard, descente dimensionnée pour arriver dans le temps du retard, remontée seulement si plus rien ne la réclame. | Un limiteur sans lookahead ne peut pas empêcher un dépassement : quand il connaît le pic, ce pic est déjà la sortie. Mesuré sur démarrage à froid en pleine échelle, pics isolés et surcharge tenue. |
| 2026-09-21 | 2 | Maximum glissant par **file monotone décroissante** (O(1) amorti) au lieu d'un balayage du retard. | Rebalayer coûterait le lookahead sur chaque sample (96 comparaisons à 48 kHz). Vérifiée contre la version naïve. |
| 2026-09-21 | 2 | Solo résolu par un **compteur partagé** (`SoloBus`) plutôt que par piste ; valeur de départ résolue au premier bloc. | Une piste n'est jamais « dé-soloée », elle est silencieuse *parce qu'une autre* est en solo : la résolution est globale. Le bus n'a pas fini de compter quand un nœud est construit, donc partir de la mauvaise valeur laisserait passer une piste soloée-out pendant toute une rampe. |
| 2026-09-21 | 2 | **Bug trouvé par un test** : `GraphPlayer::process_automated` appliquait les événements avant de récupérer le graphe entrant, donc un changement programmé dans le même bloc qu'un échange atterrissait sur le graphe sortant — puis était fondu au noir avec lui. | L'ordre importait et la documentation affirmait le contraire. Le graphe entrant est récupéré en premier. |
| 2026-09-21 | 2 | **Erreur d'attribution corrigée** : un pic de charge à 17,3 % avait été expliqué par une compilation lancée en parallèle. Le run de 30 min de la phase 3, machine au repos, atteint 12,7 % avec un p50 de 0,1 %. | L'explication était fausse. Sur 180 000 callbacks, un pic isolé de cet ordre apparaît de toute façon ; seuls p50 et p99 veulent dire quelque chose. |
| 2026-09-21 | 3 | Enveloppe ADSR en **segments linéaires**, pas exponentiels. | Un pôle n'atteint jamais zéro : il faut un seuil d'arrêt, et une voix laissée à 1e-30 est un dénormal, dont ce projet a déjà mesuré le coût. Le linéaire atterrit exactement, donc « cette voix est finie » est un fait que le pool peut exploiter. |
| 2026-09-21 | 3 | Le synthé **ne découpe pas le bloc** : il consulte sa file en arrivant sur chaque frame. Horloge partagée ajoutée au trait (`Node::set_frame`), distribuée aux deux graphes pendant un fondu. | Le plan prescrivait le découpage — nécessaire pour un nœud qui travaille par blocs, inutile pour un générateur échantillon par échantillon. Même précision, aucun coût pour le reste du graphe. Mesuré : 21 601 notes à 180 BPM, écart 0 frame. |
| 2026-09-21 | 3 | Saw et square **band-limités (PolyBLEP)**, non demandés par le plan. | Un saw naïf à 5 kHz met de l'énergie à 3 kHz par repliement, où aucune de ses harmoniques n'a affaire, et rien en aval ne peut l'en séparer. Écart mesuré par corrélation contre la version naïve. |
| 2026-09-21 | 3 | `midly` avec `default-features = false`. | Les défauts tirent `rayon` et `crossbeam` — 6 crates transitifs — pour paralléliser un parsing de quelques millisecondes, hors thread audio. Sans eux, `midly` n'a aucune dépendance. |
| 2026-09-21 | 3 | Le piège du MIDI n'est pas le format mais **l'horloge** : pistes fusionnées sur une timeline unique d'abord, puis chaque changement de tempo ne vaut que pour ce qui le suit. | Un fichier mesure le temps en ticks dont la durée dépend d'un tempo qu'il peut changer quand il veut. Lire le premier et l'appliquer partout décale tout ce qui suit, de plus en plus loin. Un format 1 met son tempo dans une piste sans notes. |
| 2026-09-21 | 4 | Trois crates au lieu d'un : `project-model` (données, commandes, undo), `project-engine` (la couture), `daw` (fenêtre). Le modèle ne dépend pas du moteur et le moteur ne connaît pas le modèle. | Un projet survit à une façon particulière de le jouer, et le thread audio n'a rien à faire d'un état éditable. Le nom `project-model` casse la convention `engine-*` exprès. |
| 2026-09-21 | 4 | Une commande se résout **une fois** en un `Edit`, et c'est l'`Edit` qui est historisé ; identifiants de piste **dérivés** (`max + 1`) plutôt que comptés. | Rejouer un `AddTrack` depuis la commande frapperait un nouvel identifiant et toutes les étapes suivantes rateraient leur cible. Un compteur serait un état de plus à restaurer à chaque undo, donc une façon de plus de le restaurer mal. |
| 2026-09-21 | 4 | `route` sépare une fois pour toutes ce qui **reconstruit** le graphe (structure, source) de ce qui **l'atteint en vol** (gain, pan, mute, solo, master). | Reconstruire à chaque mouvement de fader donnerait un fondu croisé de 10 ms par frame de souris. Un test fait passer la même modification par les deux chemins et compare les sorties. |
| 2026-09-21 | 4 | Les gestes continus **fusionnent** dans le pas qu'ils prolongent ; l'UI « scelle » au relâchement. Un interrupteur ne fusionne jamais. | Un fader tiré émet une commande par frame : ~60 entrées d'historique pour un geste, et un undo qui repasse image par image. Un mute est une décision, pas un geste. |
| 2026-09-21 | 4 | Aucune couleur nommée hors du thème, dès la première ligne d'UI ; test de contraste texte/fond par thème. | Cheap à tenir dès le départ, coûteux à imposer après. C'est ce qui fait d'un second thème une addition plutôt qu'une chasse dans chaque widget — et le contraste est la première erreur qu'un contributeur fera. |
| 2026-09-22 | 4 | **Bug trouvé en utilisant l'app** : `RefCell already mutably borrowed` au dépôt d'un fichier. Corrigé non par un `let` mais en **retirant la mutabilité intérieure** — la fabrique de sources est passée en argument à chaque appel au lieu d'être tenue par le `Studio`. | Le vrai défaut était d'avoir mis un `RefCell` là où le compilateur pouvait prouver la chose. La même erreur est désormais refusée à la compilation. Les huit tests du `Studio` passaient : ils utilisent une fabrique sans état partagé, le bug n'existait que dans le câblage réel. |
| 2026-09-22 | 4 | **Défaut de conception trouvé à l'usage** : `eject` remettait le sinus de secours, et une piste neuve bourdonnait. Une piste vide est silencieuse ; le sinus se demande. | Le sinus servait à rendre une piste audible *avant* que les fichiers marchent. Il a survécu à son utilité. Exception gardée : un fichier nommé mais non lu joue le sinus, parce qu'une piste discrètement disparue serait plus difficile à remarquer. |
| 2026-09-22 | 4 | **Second défaut à l'usage** : basculer sur le sinus jetait le chemin du fichier. Ce qu'une piste *contient* et ce qu'elle *joue* sont deux champs. | C'était le même champ, donc changer l'un perdait l'autre. Trois états choisis (silent / tone / file), aucun ne perd rien ; `eject` retire le fichier pour de bon. |
| 2026-09-22 | 4 | Le script de lancement **demande** la fermeture de la fenêtre précédente au lieu de la tuer. | Windows verrouille l'exécutable d'un processus en cours. Une terminaison forcée n'exécute pas le `Drop`, donc jette le rapport de fin : le commit qui ajoutait le confort supprimait l'observabilité que le commit précédent venait d'ajouter. |
