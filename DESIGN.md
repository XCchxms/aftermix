# Direction visuelle

Les règles suivies dans `ui/styles.css`. À lire avant d'y toucher : chacune
répond à un problème constaté, pas à une préférence.

---

## Le principe

L'application s'ouvre **par-dessus un jeu**, souvent en pleine action. Tout
découle de là : fond sombre obligatoire, information hiérarchisée pour être lue
d'un coup d'œil, aucun élément qui bouge sans raison.

## Couleur

**Des gris neutres, sans dominante colorée.** Une teinte froide appliquée à
toute l'interface — bleutée, violette — produit ce rendu « science-fiction » que
tous les utilitaires de capture se croient obligés d'adopter, et qui date
immédiatement un logiciel. Un neutre franc laisse au contenu, les vignettes, le
soin d'apporter la couleur.

**L'action principale est blanche.** Sur fond sombre, rien ne ressort davantage.
C'est aussi ce qui évite le bouton coloré saturé et le dégradé à deux teintes,
signes distinctifs d'une interface qui essaie trop.

La couleur est **réservée au sens**, jamais décorative. Quatre valeurs, pas une
de plus :

| | |
|---|---|
| **vert** `--live` | le buffer enregistre |
| **ambre** `--warn` | attention : sauvegarde en cours, écoute indisponible, son en double |
| **rouge** `--danger` | destruction ou échec |
| **bleu** `--accent` | sélection, focus, interrupteur actif, progression |

Les ombres sont **noires et discrètes**. Une ombre colorée attire l'œil sur un
bord, c'est-à-dire nulle part.

## Rayons

Trois valeurs, cohérentes entre elles : **10 px** pour un contrôle, **14 px**
pour une carte, **20 px** pour une boîte de dialogue. Un rayon qui varie sans
règle est ce qui donne l'impression d'un assemblage.

## Mouvement

Deux courbes seulement. `--ease` pour ce qui répond au doigt — court, net.
`--ease-out` pour ce qui entre en scène — plus long, décéléré. Un mouvement
uniforme partout paraît mécanique.

Trois durées : **0,12 s** pour un retour au clic, **0,18-0,22 s** pour un survol
ou un basculement, **0,26-0,42 s** pour une entrée.

Les animations d'apparition sont **plafonnées** : la cascade des cartes s'arrête
à douze, au-delà l'élégance devient une lenteur.

Rien ne bouge en boucle sauf ce qui signale un état vivant — le voyant
d'enregistrement, la marque, les vumètres.

## Typographie

Titres **resserrés** (`letter-spacing` négatif) et **graisse forte** (620-650).
C'est le détail invisible qui distingue un outil soigné d'une fenêtre système.

Toute valeur qui change en continu est en **chasse fixe**
(`font-variant-numeric: tabular-nums`) : sans cela le chiffre tressaute pendant
qu'on règle, et le regard le suit au lieu de suivre le son.

Les nombres sont écrits **à la française** — virgule décimale — et dans l'unité
où ils restent parlants : « 311 Mo » se lit mieux que « 0,3 Go ».

## Densité

C'est le **blanc qui crée la hiérarchie**, pas les traits de séparation.
Espacement resserré entre éléments d'un même groupe, large entre groupes.

Les grilles ont une **largeur maximale** : au-delà, l'œil perd la relation entre
les cartes sur un écran large.

---

## Les composants et leur raison d'être

| Composant | Pourquoi il existe |
|---|---|
| **Segmenté** (`.segmented`) | un choix parmi trois ou quatre sans menu à ouvrir ; la pastille active se déplace et l'œil suit |
| **Interrupteur** (`.switch`) | l'état se lit de loin, là où la case à cocher grise de Windows est le détail qui fait « boîte de dialogue système » |
| **Ligne de réglage** (`.field`) | le nom, ce qu'il fait, puis le contrôle. Un réglage qu'on ne comprend pas ne sera jamais touché |
| **Encadré** (`.callout`) | explique une conséquence plutôt qu'un réglage ; se distingue du texte d'aide sans crier |
| **Onglets** (`.tabs`) | onze réglages empilés se parcourent mal — on cherche au lieu de trouver |

**Les icônes sont des tracés vectoriels, jamais des glyphes de police.** Un `⚙`
se dessine différemment d'une machine à l'autre et n'est jamais aligné sur sa
ligne de base.

**La hauteur du panneau d'onglets est figée** sur la section la plus haute.
Sans cela la boîte change de taille à chaque onglet et les boutons se dérobent
sous le curseur.

**Les contrôles natifs de Windows sont neutralisés** : chevron des listes
déroulantes, croix des champs de recherche, barres de défilement. Larges et
clairs, ils trahissent immédiatement une interface web posée dans une fenêtre.

## Accessibilité

Ce ne sont pas des ajouts de confort — chacun corrige un blocage réel.

- **Anneau de focus visible au clavier seulement** (`:focus-visible`), sur tout
  ce qui se traverse : boutons, champs, listes, onglets, interrupteurs.
- **Onglets navigables aux flèches**, avec `role="tablist"`, `aria-selected` et
  `tabindex` mobile — ce qu'attend un lecteur d'écran.
- **Échap ferme** la boîte de réglages, sinon l'éditeur. Une fenêtre dont on ne
  sort qu'à la souris paraît toujours lourde.
- **Le focus revient d'où il venait** à la fermeture, faute de quoi la
  navigation au clavier repart du début de la page.
- **Les messages sont annoncés** : `role="status"` sur le voyant de buffer et
  sur le bandeau de notification.
- Chaque contrôle porte un `label` ou un `aria-label`, y compris les
  interrupteurs, dont le libellé visuel est dans une autre balise.

## Ce qui trahit un prototype

Corrigé, à ne pas réintroduire :

- **Les barres de défilement de Windows**, larges et claires.
- **Une pastille de curseur teintée**, qui se fond dans son rail. Elle est
  blanche, cerclée d'un halo coloré.
- **Un écran vide qui constate l'absence** au lieu d'expliquer le produit.
- **Une grille sans en-tête**, qui donne l'impression d'un dossier ouvert par
  accident plutôt que d'une bibliothèque.
- **Un échec silencieux.** Tout état dégradé s'affiche : raccourci refusé,
  écoute indisponible, micro débranché, piste sans son, buffer figé, plafond
  disque qui tronque le buffer. Un diagnostic invisible a déjà tué trois
  fonctionnalités sans que personne ne le sache.

## Identité des clips

Une **vignette réelle**, extraite du fichier côté Rust à la sauvegarde et
conservée à côté du clip. Elle n'est jamais produite dans la vue : un élément
`<video>` par carte garde le fichier ouvert, mobilise un décodeur et dégradait
la lecture dans l'éditeur.

Le fond reste neutre tant qu'elle n'est pas prête. Une couleur vive qui
disparaîtrait ensuite ferait clignoter la grille au chargement.

Les clips antérieurs à cette fonction sont **rattrapés au lancement**, en fond :
la grille s'affiche d'abord, les images apparaissent ensuite. Mesuré sur
30 clips, moins de 30 s pour la totalité. Attendre la fin figerait la fenêtre au
démarrage pour une amélioration purement visuelle.

## Rien ne doit pouvoir pousser l'interface hors du cadre

Trois débordements corrigés, tous dus à du contenu de longueur imprévisible :

- Le **voyant d'état** liste les pistes détectées : tronqué à 42 caractères,
  sinon il repoussait les boutons hors de la fenêtre.
- Le **nom du clip** dans l'éditeur : tronqué, sinon il élargissait le panneau.
- Les **actions du panneau** (Exporter, Supprimer) sont collées en bas et
  restent visibles quand la liste de pistes défile. Sur six pistes, Exporter
  sortait du champ et l'on croyait le bouton absent.

## Adaptation aux tailles

Trois paliers, pas plus :

| | |
|---|---|
| **≥ 1600 px** | la grille respire au lieu de s'étirer |
| **≤ 1080 px** | le mixage passe sous le lecteur — une colonne de 360 px sur 900 px de large ne laisse plus rien à la vidéo |
| **≤ 760 px** | l'en-tête se réorganise, la recherche prend toute la largeur, les onglets se resserrent |

## Ce qui reste à faire

- Mode clair, si un usage hors jeu le justifie un jour.
