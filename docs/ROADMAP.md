# Roadmap — vers un DAW complet et compétitif

Liste technique non exhaustive, établie le 2026-09-23 sans tenir compte du temps
ni de la difficulté : ce qui sépare Daw d'un DAW professionnel. L'ordre de travail
vit dans [`plan-daw-rust.md`](../plan-daw-rust.md) ; ce fichier dit *quoi*, le
plan dit *quand* et *pourquoi*.

Légende : `[x]` fait · `[~]` partiel · `[ ]` à faire.

Les limites qui ne sont pas du code, et qui cadenceront le reste : l'écoute
(la qualité d'un effet se juge à l'oreille), le matériel (clavier MIDI,
interface audio, Mac), le contenu sonore (samples, presets), les licences de
certaines dépendances, et de vrais utilisateurs.

## 1. Moteur audio
- [x] Callback temps réel sans allocation ni verrou, échange de graphe crossfadé, automation au sample près
- [ ] Traitement multicœur du graphe (branches indépendantes en parallèle, work-stealing)
- [ ] Compensation de latence (PDC) sur tous les chemins
- [~] Moteur de rendu hors temps réel, plus rapide que le temps réel et déterministe (export, freeze, bounce)
- [ ] Streaming disque des longs fichiers au lieu d'un décodage complet
- [ ] Pilotes ASIO (Windows), CoreAudio (macOS), JACK / PipeWire (Linux) ; changement de périphérique et de buffer à chaud, hot-plug
- [ ] Entrées audio, monitoring, compensation de latence d'enregistrement
- [ ] Protection dénormaux (FTZ/DAZ), suréchantillonnage des non-linéarités, bus de mix 64 bits
- [ ] Rééchantillonnage haute qualité, dither à l'export

## 2. Temps et synchronisation
- [x] Tempo map en marches, signature, conversion exacte ticks ↔ frames
- [ ] Rampes de tempo, changements de signature
- [ ] Marqueurs, région de boucle, punch in/out, métronome et décompte
- [ ] Swing et groove templates
- [ ] Ableton Link, MIDI clock in/out, MTC/LTC, synchronisation vidéo

## 3. Édition audio
- [~] Clips, déplacement, rognage, clips liés
- [ ] Time-stretch et pitch-shift avec préservation des transitoires
- [ ] Détection de transitoires et de tempo, warp markers, quantification audio
- [ ] Slice vers sampler ou MIDI
- [ ] Fades et crossfades à courbes, gain et enveloppe par clip, reverse, normalize
- [ ] Take lanes et comping, non destructif
- [ ] Audio → MIDI (mélodie, harmonie, batterie), détection de hauteur
- [ ] Édition spectrale

## 4. MIDI
- [x] Clips, notes, vélocité, import `.mid`, séquenceur au sample près avec chase
- [ ] Entrée live, enregistrement (overdub, loop record, capture rétroactive), MIDI learn
- [ ] Couloirs CC, pitch bend, aftertouch ; MPE et MIDI 2.0
- [ ] Quantification (force, swing), humanize, multi-sélection, copier/coller, transposition
- [ ] Outils de gammes et d'accords, step sequencer, éditeur de batterie, notation
- [ ] Effets MIDI : arpégiateur, accords, aléatoire, gamme
- [ ] Export `.mid`

## 5. Instruments
- [~] Synthé un oscillateur (sine/saw/square PolyBLEP) + ADSR, polyphonie à vol de voix, instrument par piste
- [ ] Sampler : multisamples, zones, round-robin, streaming
- [ ] Drum rack : pads, choke groups, chaînes par pad
- [ ] Synthés wavetable, FM, soustractif avec filtres ZDF
- [ ] Matrice de modulation (LFO, enveloppes, macros), glide, modes de voix, MPE

## 6. Effets
- [x] EQ 4 bandes, compresseur avec indicateur de réduction, delay synchronisé, reverb Freeverb
- [ ] Filtres ZDF, saturation et distorsion suréchantillonnées, chorus, flanger, phaser
- [ ] Gate et expander, compresseur multibande, limiteur true peak, transient shaper
- [ ] Reverb à convolution (FFT partitionnée), reverb algorithmique FDN
- [ ] EQ linéaire en phase, EQ dynamique, traitement M/S
- [ ] Vocoder, correction de hauteur
- [ ] Analyseurs : spectre, LUFS, true peak, corrélation

## 7. Mixage et routage
- [x] Volume, pan, mute, solo, bus d'effets avec envois post-fader, retours solo-safe
- [ ] Pistes de groupe et dossiers, routage libre, sidechain
- [ ] Envois pré-fader, VCA, vumètres par tranche
- [ ] Freeze et flatten, chaîne de mastering
- [ ] Spatial et surround

## 8. Automation et modulation
- [~] Automation au sample près dans le moteur (absente du modèle)
- [ ] Couloirs d'automation pour tout paramètre, courbes, modes read/write/touch/latch
- [ ] Enregistrement depuis un contrôleur, enveloppes de clip
- [ ] Modulateurs (LFO et enveloppes sur tout paramètre)

## 9. Plugins
- [ ] Hôte CLAP, hôte VST3 (COM, IPlugView), AU sur macOS
- [ ] Isolation par processus
- [ ] Scan avec liste noire, presets, état dans le projet
- [ ] Fenêtres des plugins intégrées par OS, latence déclarée (PDC), bus sidechain

## 10. Performance live
- [ ] Session View : grille de clips, scènes, quantification de lancement, follow actions
- [ ] Enregistrement dans les slots, crossfader
- [ ] Scripts de contrôleurs, retours LED et écran

## 11. Données et projet
- [x] Format versionné et migré, identité par hash, undo complet, garde des changements non sauvegardés
- [ ] Sauvegarde automatique et récupération après crash (journal des commandes)
- [ ] Re-lien par hash, collecte des fichiers dans le projet, archive
- [ ] Templates, export des stems, import DAWproject, AAF/OMF
- [ ] Diff et fusion de projets, collaboration (CRDT, local-first)
- [ ] Performances sur très gros projets

## 12. Interface
- [~] Timeline, éditeur de notes, mixeur, panneaux EQ et FX, thèmes
- [ ] Rendu fluide à grande échelle, multi-sélection partout
- [ ] Navigateur de fichiers et de samples avec pré-écoute, tags, recherche
- [ ] Glisser-déposer entre toutes les zones, fenêtres détachées, multi-écran
- [ ] Raccourcis personnalisables, panneau d'historique, aide contextuelle
- [ ] Accessibilité, localisation, HiDPI

## 13. Export
- [~] WAV (16 bits avec dither, 24 bits, 32 bits flottant), rendu hors temps réel
- [ ] AIFF, FLAC, MP3, AAC ; normalisation LUFS
- [ ] Stems, multipiste, export d'une région

## 14. Plateformes et diffusion
- [~] Linux et Windows (WASAPI), une seule machine testée
- [ ] macOS (CoreAudio, notarisation, signature), ASIO
- [ ] Installateurs, mises à jour, rapport de crash, télémétrie consentie
- [ ] Licences et activation si commercial

## 15. Qualité
- [x] Tests unitaires et de propriétés, no-alloc dans le callback, CI Linux + Windows, fixtures de chaque version du format
- [ ] Null tests et rendus de référence pour chaque effet et instrument
- [ ] Fuzzing des formats (projets, audio, MIDI, presets)
- [ ] Tests de charge, benchmarks surveillés en CI
- [ ] CI macOS, matrice matérielle

## Les cinq chantiers les plus durs

1. Time-stretch de qualité (vocodeur de phase, transitoires, formants).
2. Hébergement de plugins avec isolation par processus.
3. Moteur multicœur déterministe avec compensation de latence.
4. Session View avec warping.
5. Collaboration temps réel (CRDT sur le modèle, synchronisation des fichiers).
