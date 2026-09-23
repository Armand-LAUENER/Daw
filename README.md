# Daw

Moteur audio temps réel en Rust, avec pour but une station audio utilisable et
performante. Chaque phase a un critère de sortie mesurable — on ne passe à la
suivante qu'une fois le critère vérifié par une mesure, pas par une impression.

**Règle centrale, non négociable** : le thread audio (callback `cpal`) ne fait jamais
d'allocation, de lock, d'I/O ni d'attente bloquante. Toute communication avec ce thread
passe par des structures lock-free. C'est outillé, pas seulement documenté :
`assert_no_alloc` en debug et les tests `no_alloc` font échouer la CI si la règle est
violée.

## État

| phase | état |
|---|---|
| 0 — Fondations temps réel | **validée** — 30 min sans xrun (WASAPI natif) |
| 1 — Lecture mono-piste + filtre | **validée** — 10 min, 119 seeks, aucune discontinuité |
| 2 — Graphe multi-piste | **validée** — 30 min, 16 pistes, 266 475 mouvements de fader, 78 échanges de graphe, 0 xrun |
| 3 — Séquenceur & MIDI | **à moitié** — 21 601 notes à 180 BPM, écart **0 frame** ; le MIDI externe est écrit mais non validé (pas de clavier) |
| 4 — Interface graphique | **utilisable** — timeline, éditeur de notes, mixeur, transport, undo ; critère de sortie partiellement mesuré |
| 5 — Effets intégrés | **faits** — EQ, compresseur, delay, reverb ; la suite de tests par sweep + FFT reste à écrire |
| 6 — Persistance | **faite** — format v9, chaque version antérieure relue et migrée en test |
| 7 — Hébergement CLAP | optionnelle |

Le détail des mesures et des pièges d'environnement est dans
[`docs/REPRISE.md`](docs/REPRISE.md) — à lire en premier en reprenant le projet.
Le plan, l'ordre révisé et le journal de bord sont dans
[`plan-daw-rust.md`](plan-daw-rust.md) ; ce qui sépare Daw d'un DAW complet est
listé dans [`docs/ROADMAP.md`](docs/ROADMAP.md).

## Ce que Daw sait faire

- **Arrangement** : clips audio et MIDI sur une timeline, déplacés, rognés,
  dupliqués en copie ou en clip lié ; waveforms ; tempo map à plusieurs changements ;
  région de boucle (glisser sur la règle avec Majuscule) et métronome.
- **MIDI** : éditeur de notes avec vélocité, écoute des notes à l'édition, import de
  `.mid`, un instrument par piste (oscillateur sine/saw/square + ADSR).
- **Mixage** : volume, pan, mute, solo, EQ paramétrique 4 bandes, compresseur avec
  indicateur de réduction de gain, delay synchronisé au tempo, reverb Freeverb, bus
  d'effets avec envois, limiteur sur le master.
- **Projet** : ouvrir, enregistrer, enregistrer sous, nouveau ; undo/redo complet ;
  garde des changements non sauvegardés ; audio identifié par hash.
- **Export** : WAV 16 bits (dither TPDF), 24 bits ou 32 bits flottant, rendu hors
  temps réel en arrière-plan.

## Lancer

L'audio ne fonctionne qu'en Windows natif : WSLg décroche après ~70 s (cause
identifiée, voir le journal). Les tests hors temps réel tournent sous WSL.

```sh
cargo test --workspace          # tout ce qui ne demande pas de périphérique
scripts/check.sh                # fmt, clippy, tests (no-alloc actif), doc — aussi le hook de pré-push
scripts/windows.sh check        # clippy -D warnings + tests, sur Windows
scripts/windows.sh app          # la fenêtre
scripts/windows.sh phase2 --tracks 16 --minutes 30 --swap-every 23
scripts/windows.sh phase3 --self-test --bpm 180 --minutes 30
```

Dans la fenêtre : dépose des fichiers audio ou `.mid` dessus pour créer des pistes
et des clips.

| raccourci | action |
|---|---|
| `Espace` | lecture / pause |
| `Ctrl+Z` / `Ctrl+Maj+Z` | annuler / rétablir |
| `Ctrl+S` | enregistrer |
| `Ctrl+D` | dupliquer le clip sélectionné |
| `Suppr` / `Retour arrière` | supprimer le clip ou la note sélectionnés |
| `Maj` + glisser sur la règle | dessiner ou redimensionner la région de boucle |

## Structure

Huit crates, séparés par ce qu'ils ont le droit de connaître.

```
crates/
├── engine-dsp/     DSP pur, sans I/O, testable sans carte son
│   ├── oscillator.rs      Osc : sine, et saw/square band-limités (PolyBLEP)
│   ├── biquad.rs          RBJ passe-bas/passe-haut, forme directe II transposée
│   ├── smoothed_biquad.rs coefficients interpolés (pas de zipper au balayage)
│   ├── eq.rs              une bande d'EQ, déplaçable pendant la lecture
│   ├── dynamics.rs        compresseur feed-forward, genou doux
│   ├── delay.rs           ligne à retard avec feedback plafonné
│   ├── reverb.rs          Freeverb
│   ├── smoother.rs        LinearSmoother : rampe dérivée de la cible
│   └── adsr.rs            segments linéaires, donc une voix finie est un fait
├── engine-core/    le graphe : ce qui tourne dans le callback
│   ├── buffer.rs          AudioBuffer planaire, emprunté et non possédé
│   ├── graph.rs           ordre topologique, latence du plus long chemin
│   ├── node.rs            Node, Gain, Pan (puissance constante), Mute, Solo + SoloBus
│   ├── eq.rs, effects.rs  EQ, compresseur, delay et reverb comme nœuds
│   ├── limiter.rs         lookahead 2 ms, garantie exacte, maximum glissant O(1)
│   ├── synth.rs           pool de voix, notes au sample près, patchs d'instrument
│   ├── sequencer.rs       joue les notes de la timeline sur l'horloge du transport
│   ├── transport.rs       play/pause/stop/seek, position publiée en atomiques
│   ├── automation.rs      changements de paramètres datés
│   ├── event.rs           file SPSC générique d'événements datés
│   └── swap.rs            échange de graphe atomique, fondu croisé de 10 ms
├── engine-io/      cpal, le thread audio et ses diagnostics
├── engine-io-file/ décodage WAV/FLAC/MP3/AAC, rééchantillonnage + transport de fichier
├── engine-midi/    fichiers .mid (midly) et clavier (midir, derrière une feature)
├── project-model/  ce qui s'édite et se sauvegarde — sans dépendance au moteur
│   ├── arrangement.rs     clips, placements, notes
│   ├── time.rs            ticks entiers et tempo map
│   ├── asset.rs           pool de médias identifiés par hash
│   ├── command.rs         la seule façon de modifier un projet
│   ├── history.rs         undo/redo, gestes continus fusionnés
│   └── file.rs            format texte déterministe, versionné et migré
├── project-engine/ la couture : compile un projet en graphe, ticks → frames, aiguille les édits
└── daw/            la fenêtre (eframe/egui) : timeline, éditeur, mixeur, export
```

**Pourquoi `project-model` ne s'appelle pas `engine-project`** : tout l'enjeu est que
l'état projet ne soit *pas* l'état moteur. Le premier est édité, annulé, écrit sur
disque ; le second change à chaque échantillon dans le thread audio. Ils se
synchronisent par messages, jamais par une structure partagée.

## Les deux chemins vers le thread audio

C'est la décision qui structure tout le reste, et `project-engine::route` est le seul
endroit où elle se prend :

| ce qui change | chemin | coût |
|---|---|---|
| structure (piste ajoutée, retirée, source changée) | graphe recompilé, échangé | fondu croisé de 10 ms |
| paramètre (gain, pan, mute, solo) | événement daté | au sample près, gratuit |

Confondre les deux coûterait un fondu de 10 ms par frame de souris.

## Ce que le projet a appris

Le CPU n'a jamais été la contrainte : 16 pistes avec automation coûtent 1,2 % d'un
buffer de 10 ms. Les défauts sérieux portaient tous sur **l'état** — deux dépositaires
de la même vérité qui divergent — et pas sur la vitesse. Voir la section du même nom
dans le plan.
