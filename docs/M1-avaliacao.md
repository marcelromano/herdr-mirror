# M1-win — avaliação das branches de orfaos (28/10) + serie/m1

Base de referência: `win/feat/windows-server-mode` @5007a40 (fonte do binário em
produção: host kind `windows`, `--windows-host`, relay fixo). `win/main` = 195398c.

## O que cada branch resolve

### serie/m1 (454e5e5, 1eee7bc — sobre 195398c, sem suporte Windows)
- **Resolve o núcleo do contrato M1**: quando o pane remoto some, a copia local e
  fechada pelo id guardado no mapa remoto→local (`state.panes[rid].local_id`),
  nunca por id recalculado. `plan_pane_reap` (puro, testavel) + `execute_pane_reap`
  em `src/mirror.rs`; integrados em `converge_inner` (loop `gone_panes`) e em
  `apply_remote_closes` (evento `pane.closed`).
- Guardas do contrato: id vazio/compartilhado/ambiguo => `Refuse` (log, nao fecha);
  pane com agente vivo que nao e copia do espelho => `Refuse`; entrada do mapa so
  cai quando o fechamento se resolve (falha transitória => retenta no proximo passe).
- 1eee7bc: a copia e achada **pela chave do streamer** (`pane.process_info`, argv
  `herdr-mirror pane <alvo> <rid>`) quando o id guardado esta obsoleto: 1 achado =>
  fecha; 0 => id do mapa/`Absent`; >1 => `Refuse`. Mais varredura de orfaos
  (`plan_orphan_sweep`): pane-espelho que streama um rid que nem esta no mapa nem
  no remoto e fechado so no **segundo passe consecutivo** (`orphan_seen` no state).
- Testes: socket falso, sem Herdr real nem ssh (todos os 4 cenarios do contrato +
  e2e com id obsoleto).
- **M1b (marcar a copia com o rid ao criar): NAO cobre.** A chave e o argv vivo do
  streamer; quando o streamer morre, a chave morre junto.
- **Shells com streamer morto: NAO cobre.** `plan_orphan_sweep` so ve panes cujo
  streamer ainda esta vivo (a chave vem do argv); orfao com streamer morto e limpo
  a mao (o estado descrito em coord/STATE.md).
- Sem suporte Windows (base 195398c): nao tem kind `windows`, `--windows-host`,
  relay fixo.

### win/fix/ssh-advisory-not-a-fault (fc1d910 — 1 commit sobre 195398c)
- Filtra os avisos `**` do OpenSSH (post-quantum) do stderr do relay para nao
  tratar conexao sadia como falha.
- **Ja esta na base**: `win/feat/windows-server-mode` tem o mesmo fix como 6748af1
  (mesma funcao `relay_fault`, mesmo teste; o arquivo na base e um superconjunto
  com o transporte Windows). Nao ha nada a integrar.

### win/fix/orphan-mirror-shells (355c0e8, 7325303, 3c1e020 — sobre 5007a40)
- 355c0e8: **lock de converge por host** entre daemon e `once` (a corrida era a
  origem dos shells orfaos: os dois criavam pane para o mesmo remoto, o ultimo
  `save_state` vencia e o perdedor ficava sem mapa e sem streamer).
- 7325303: `plan_orphan_gc` — fecha shells orfaos: pane **sem mapa**, dentro de
  workspace-espelho vivo, ainda no cwd placeholder `.mirror-pane`, sem agente e
  **sem streamer vivo (pidfile)**; so no segundo passe (`orphan_candidates`);
  limite 10/passe; nunca esvazia workspace nem tab que o mapa reivindica.
- 3c1e020: `status` mostra shells orfaos por host.
- **M1b: NAO cobre** (identidade por cwd placeholder + ausencia de streamer, sem
  rid marcado).
- **Shells com streamer morto: COBRE** (`!streamer_alive` e requisito).
- Nao toca no caminho M1 (id guardado/streamer key): o loop `gone_panes` continua
  fechando cegamente por `entry.local_id` (o bug do contrato).

### win/fix/mirror-lock-status (773bb49, 672c5fc, 7354da4, cffe241 — sobre 5007a40)
- 773bb49: **o mesmo commit de lock de 355c0e8** (arvores idênticas).
- 672c5fc: lock indisponível => **pular o passe (fail-closed)**, nao convergir sem
  lock; teste de exclusao entre processos.
- 7354da4: contenda do lock do daemon no Windows (`fs2::lock_contended_error`)
  lida como "ja rodando", nao erro de I/O.
- cffe241: `status` com suspects/protecoes de orfaos **somente leitura**
  (`orphan_status`) — nao fecha nada.
- **M1b: NAO cobre. Shells com streamer morto: NAO coleta** (apenashints).
- Nao toca no caminho M1.

### win/fix/mirror-orphan-gc (773bb49, 672c5fc, 7354da4, cffe241, 95f9c75 — sobre 5007a40)
- **Superconjunto linear de mirror-lock-status** (mesmos 4 commits) + 95f9c75:
  `src/orphan_gc.rs` — coleta de shells orfaos **so no daemon** (`deps.orphan_gc`),
  **fail-closed** e com **evidencia fresca do proprio daemon**: inventario
  validado (contagens ws/tab/pane/layout, ids unicos), identidade por
  `terminal_id` + marker canonicalizado, `idle` via `pane.process_info` (exatamente
  1 processo em primeiro plano = o shell, sem filhos), MIN_AGE 30s / MAX_GAP 180s,
  limite 10/passe, e **recontagem fresca + `pane.get` + `process_info` antes de
  cada close**. Qualquer inconsistência => pula/zera `orphan_candidates`.
- **M1b: NAO cobre** (mesma identidade por terminal_id/cwd, sem rid marcado).
- **Shells com streamer morto: COBRE** (o requisito `idle` exige exatamente o
  shell, sem filhos — streamer morto + shell parado).
- Nao toca no caminho M1.

## Sobreposicao / conflito com M1

| branch | nucleo M1 (id guardado) | varredura de orfaos | lock converge | conflito com serie/m1 |
|---|---|---|---|---|
| serie/m1 | sim (e o fix) | streamer vivo, 2 passes | nao | — |
| ssh-advisory | nao | nao | nao | duplicado na base (6748af1) |
| orphan-mirror-shells | nao | streamer morto, snapshot antigo, daemon+once | sim (355c0e8) | campos de state distintos (`orphan_candidates` vs `orphan_seen`); regioes distintas de `converge_inner` |
| mirror-lock-status | nao | somente leitura | sim (fail-closed) | idem |
| mirror-orphan-gc | nao | streamer morto, evidencia fresca, so daemon | sim (fail-closed) | idem |

As mudancas de serie/m1 (loop `gone_panes`, `apply_remote_closes`, novos helpers,
`orphan_seen`) e as da linha win (lock em `converge`, GC no fim de `converge_inner`,
`orphan_candidates`) **se complementam e nao colidem**: uma fecha a copia **com
mapa** cujo remoto sumiu (id guardado/chave do streamer); a outra fecha o shell
**sem mapa** com streamer morto. Juntas cobrem os 5 orfaos do sintoma do SUP e os
orfaos com streamer morto.

## M1b e shells com streamer morto

- **Nenhuma branch implementa M1b literal** (marcar a copia com o rid na
  criacao, ex.: titulo/env). M1b segue em backlog.
- O **efeito pratico** de M1b (chave que sobrevive a morte do streamer) fica
  coberto pela combinacao: copia **com mapa** => id guardado no mapa (serie/m1);
  shell **sem mapa** => GC por terminal_id/cwd/`idle` (mirror-orphan-gc).

## Conjunto recomendado

1. **Base**: `win/feat/windows-server-mode` @5007a40 (preserva kind `windows`,
   `--windows-host`, relay fixo; ja contem o fix ssh-advisory).
2. **Integrar**: `win/fix/mirror-orphan-gc` (5 commits lineares sobre a base —
   cherry-pick limpo). Escolhe-se ele e nao `orphan-mirror-shells` porque:
   - o commit de lock e identico (355c0e8 ≡ 773bb49);
   - o GC do orphan-gc e estritamente mais conservador: so no daemon, fail-closed,
     evidencia fresca revalidada antes de cada close; o GC do orphan-mirror-shells
     roda tambem no `once` e decide sobre o snapshot **antigo** do inicio do passe;
   - o fail-closed (pular o passe sem lock) e mais seguro que "convergir sem lock".
3. **Portar de serie/m1**: so o nucleo M1, que nenhuma branch da linha win cobre —
   `plan_pane_reap`/`execute_pane_reap`, chave do streamer
   (`streamer_key`/`collect_streamer_keys`), varredura de orfaos 2-passes
   (`plan_orphan_sweep` + `orphan_seen`), integracao em `converge_inner` e
   `apply_remote_closes`, helper de teste `connect_unchecked` e todos os testes.
4. **Descartar**: `win/fix/orphan-mirror-shells` (suplantado por
   mirror-orphan-gc) e `win/fix/ssh-advisory-not-a-fault` (ja na base).
