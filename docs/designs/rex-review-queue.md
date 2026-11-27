# rex-review-queue

*   **Autor:** Cirdes
*   **Status:** Approved
*   **Data:** 2026-11-27

--------------------------------------------------------------------------------

## Overview

O rex-review-queue mantém pronta, no macmini, uma git worktree para cada pull request em que alguém pediu review ao autor, com o setup do repo já rodado. Um painel TUI num bloco do Rex lista essas reviews. O CLI se chama `rrq`. Com **Enter**, o painel leva o app do Rex no MacBook até uma sessão aberta naquela worktree. Com **`c`**, faz o mesmo e abre também o Claude e o reviewr. É o port do herdr-reviewq para o Rex, sem o Herdr.

Contexto: [spike_results.md](spike_results.md) (Estágio 0, veredito: seguir) e o código do herdr-reviewq em `~/Workspaces/herdr-reviewq`.

### Termos

*   **Herdr:** multiplexador de terminal para agentes que o autor usa hoje, em duas versões: TUI e app desktop (herdr-app).
*   **herdr-reviewq:** o projeto atual (Rust, `~/Workspaces/herdr-reviewq`). É um daemon macOS que transforma a fila de reviews do GitHub em worktrees prontas e tem um painel como plugin do Herdr.
*   **Rex:** o multiplexador de terminal para onde o autor está migrando. Tem um servidor, que guarda sessões e processos, e apps clientes (Mac e iPhone) que se conectam a servidores locais ou remotos ([docs](https://www.superlogical.com/rex/docs/llms.txt)).
*   **Modelo do Rex:** servidor → **sessão** (aparece na sidebar do app) → **window** (uma aba da sessão) → **bloco** (um pane com um terminal). Os apps conectados são **clientes**, cada um com um `label` definido no app (o MacBook do autor usa `macbook`). Um processo fala com o servidor pelo CLI `rex`, por exemplo `rex api call <método> <json>`.
*   **reviewr:** TUI de revisão de diff que o autor usa ao lado do agente. Roda como `reviewr <worktree>`. Fica no fork `cirdes/herdr-reviewr`, branch `rex-backend`, que já funciona no Rex.
*   **OSC 7501 (Program Status Protocol):** sequência de escape com que um programa informa ao terminal o próprio estado. Cada bloco guarda um **registro raiz** e **registros-filho** identificados por `id`, cada um com `state` (`idle`, `working`, `done`, `blocked`, `error`; `clear` apaga), `app` e `title`/`msg` em base64. O app do Rex junta os registros de um bloco num único indicador na aba, visível quando ela não está em foco ([protocolo](https://www.superlogical.com/rex/docs/build/program-status)). Os registros `done` e `error` sobrevivem ao fim do processo que os emitiu.
*   **`rrq`:** o binário do projeto. `rrq start`, `rrq stop` e `rrq restart` controlam o daemon (LaunchAgent). `rrq status` mostra o daemon, a fila e os alertas. `rrq tui` abre o painel no terminal atual. `rrq doctor` diagnostica. `rrq update` baixa o último build da `main`. A TUI não inicia nem para o daemon.
*   **Tick e ciclo de sync:** o loop do daemon roda um **tick** a cada 1 s (`daemon.rs:910-913`). O **ciclo de sync** (GitHub, reconcile, executor) e o passo de **extras** (CI, stacks, "Início") rodam a cada `poll_interval_secs`, 60 s por padrão.
*   **Painel:** o TUI do projeto. Seções: Iniciados (reviews em que um agente já foi aberto), Pendentes, Meus PRs (PRs do autor), Adotados, Sem sync (repo cujo último sync com o GitHub falhou) e Saindo (PR fora da fila, esperando a carência). Colunas: PR, ícone de estado, Estado, CI, Início (quando um agente foi visto na worktree pela primeira vez), Autor, Empilhado (posição numa pilha de PRs) e Título.

### Garantias herdadas

Estas regras do herdr-reviewq continuam valendo sem mudança de comportamento:

*   **Worktree intocada:** sem commits locais, sem troca de branch e sem arquivos alterados ou novos, fora os ignorados listados em `disposable_ignored`. É o que os **facts** (estado git coletado antes de remover) verificam.
*   **Adoção:** uma worktree adotada nunca é removida pelo daemon. Ela pode ser adotada à mão (tecla `a` no painel) ou de forma automática, quando o daemon vai removê-la e os facts mostram que ela divergiu.
*   **`disposable_ignored`:** a lista, por repo, de caminhos ignorados que o setup cria (por exemplo `node_modules`). São os únicos ignorados que podem ser apagados com a worktree.
*   **Commits novos no PR:** a worktree é atualizada com `git reset --keep`, que recusa sobrescrever mudanças locais.
*   **Remoção:** acontece só depois de 15 min fora da fila (a carência). Antes de remover, o daemon faz backup do head, nunca usa `--force` e só apaga a branch local se foi ele quem a criou e ela não mudou desde então.

## The Problem

O autor está trocando o Herdr pelo Rex e vai parar de rodar o Herdr. O herdr-reviewq só funciona sobre o Herdr. Sem um port, a fila de reviews volta a ser manual: criar worktree, rodar `mise`/`bundle`/`pnpm`, lembrar de limpar depois.

Além disso, o modo remoto do herdr-reviewq (macmini como servidor, MacBook como tela) depende de um mod JS no herdr-app e de uma bridge que, a cada 1 s, faz polling de um documento intermediário para saber que workspace focar. No Rex, o app do MacBook se conecta ao servidor do macmini por ssh, e um processo no macmini troca a sessão mostrada no MacBook com uma chamada de API (~100 ms no spike).

A solução é um fork do herdr-reviewq. Sync com o GitHub, worktrees, setup, reconcile, CI, stacks e o painel são reaproveitados. A camada do Herdr é trocada pela API do Rex, e o projeto passa a usar o que o Rex oferece: status por OSC 7501, sessões criadas e trocadas por API, e reabertura do painel depois de um restart.

## Goals

*   **Paridade do painel com o herdr-reviewq.** Mesmas seções, colunas, contadores, busca `/` e ações (`sync-now`, adotar, copiar URL). O autor ajusta o painel com frequência, então o código do TUI é reaproveitado, não reescrito.
*   **Mesmas garantias sobre worktrees** (ver Garantias herdadas).
*   **Tudo roda no macmini.** Daemon, worktrees, servidor Rex e painel ficam lá. O MacBook só roda o app do Rex.
*   **Enter rápido.** No painel, Enter cria (ou reencontra) a sessão do PR e troca o app do MacBook para ela em **menos de 500 ms**, medidos da tecla até a resposta do `session.select`. O Enter não roda git nem setup: a worktree já está pronta.
*   **Combo com `c`.** No painel, `c` faz o mesmo que o Enter e abre também o Claude e o reviewr na sessão. Se um deles já está aberto, não duplica. Se o bloco existe mas o programa saiu, relança o programa.
*   **Avisos no MacBook sem depender de `osascript`.** "Review pronta", falha de setup e erros do daemon viram registros OSC 7501. O spike confirmou que aparecem como **indicador na aba `reviews` da sidebar do MacBook**. Ainda falta confirmar se o texto aparece na Activity e se o macOS notifica (ver Open Questions).
*   **O painel é aberto pelo autor.** `rrq tui` num bloco qualquer (o lugar natural é uma sessão `reviews`). O daemon não abre nem reabre o painel. Ao abrir, o painel emite de novo os status a partir do estado salvo.

### Critérios de sucesso

*   Uma semana inteira de reviews sem nenhum processo do Herdr rodando.
*   Nenhuma worktree adotada ou suja perdida na troca.
*   O tempo entre o pedido de review e a worktree pronta não piora em relação a hoje (linha de base: ver Open Questions).

## Non-Goals

*   **Suporte ao Herdr.** Nenhum backend Herdr no rex-review-queue. O herdr-reviewq fica congelado: correções só por cherry-pick, se houver.
*   **Daemon em mais de uma máquina** ou no MacBook.
*   **Trocar o painel pela palette do Rex.** Um picker na palette pode vir depois, como atalho.
*   **Crate compartilhado** entre herdr-reviewq e rex-review-queue.
*   **Indicador por PR na sidebar.** O Rex mostra um indicador por aba (ver Termos). O projeto não cria uma aba por PR só para isso.
*   **Na primeira entrega:** picker na palette, tecla global via `init.lua`, "Início" por eventos (`rex events`), retomada automática de programas pelo OSC 7577 (Program Resume Protocol).
*   **Persistência do servidor Rex no macmini.** É configuração da máquina (Rex.app como login item), feita uma vez pelo autor (ver Open Questions sobre auto-login).

## Critical User Journeys (CUJs)

### CUJ 1: Review nova fica pronta

*   **Given**: o daemon roda no macmini, o painel está aberto na sessão `reviews` e o app do MacBook está conectado ao host `macmini`.
*   **When**: alguém pede review ao autor num PR de um repo configurado.
*   **Then**: em até 60 s (um ciclo de sync), o daemon cria a worktree e roda o setup. O PR aparece em Pendentes com `◐ preparando` e depois `● pronto`. O painel emite um registro `done` para o PR, e a aba `reviews` ganha o indicador na sidebar do MacBook. Nenhuma sessão nova aparece.

### CUJ 2: Entrar na review (Enter)

*   **Given**: um PR está `● pronto` no painel.
*   **When**: o autor busca o PR com `/` e aperta Enter.
*   **Then**: em menos de 500 ms, existe uma sessão `#123 repo` no macmini com um shell na worktree, ela aparece na sidebar e o app do MacBook troca para ela. Se a sessão já existia, o painel só troca.

### CUJ 3: Entrar e abrir o combo (`c`)

*   **Given**: um PR está `● pronto` no painel.
*   **When**: o autor aperta `c` sobre o PR.
*   **Then**: o mesmo que o CUJ 2 (o combo não tem a meta de 500 ms). A sessão ganha o Claude num bloco e o reviewr ao lado, e o foco termina no Claude. Um segundo `c` não duplica nada. Se o Claude tinha saído (`/exit`), ele é relançado no mesmo bloco.

### CUJ 4: PR sai da fila sem sessão em uso

*   **Given**: o PR tem worktree intocada e uma sessão só com shells parados na worktree.
*   **When**: o autor envia a review, ou o PR é fechado ou mergeado.
*   **Then**: o PR vai para Saindo (`sai em Nmin`). Depois de 15 min, o daemon fecha a sessão e remove a worktree.

### CUJ 5: PR sai da fila com sessão em uso

*   **Given**: o PR saiu da fila, a carência passou e o Claude continua aberto na sessão.
*   **When**: o daemon tenta remover.
*   **Then**: nada é fechado nem removido. O PR fica em `Saindo · sessão em uso`, com o tempo retido, e a sessão continua na sidebar do Rex. Quando o Claude sai, a remoção acontece no ciclo de sync seguinte. Se o autor fechar a sessão à mão, a remoção também segue.

### CUJ 6: Setup falha

*   **Given**: um PR novo entrou na fila.
*   **When**: um passo do setup (por exemplo `bundle install`) falha ou estoura o timeout.
*   **Then**: o PR aparece como `✗ falhou · <passo>`, o painel emite `error` com "Setup falhou: <passo>" e a aba `reviews` ganha o indicador. `l` mostra o log do setup.

### CUJ 7: Painel cai

*   **Given**: o painel está aberto e o TUI termina por panic ou erro.
*   **When**: o TUI sai.
*   **Then**: antes de sair, ele emite um registro `error` com o motivo (no caminho de erro e num panic hook). O bloco volta para o shell, o registro sobrevive e a aba mostra o indicador no MacBook. O autor digita `rrq tui` de novo. O daemon continua sincronizando nesse meio-tempo.

### CUJ 8: Migração do herdr-reviewq

*   **Given**: o herdr-reviewq roda hoje no macmini, com worktrees pendentes, adotadas e sujas.
*   **When**: o autor segue os passos do README (parar o serviço antigo, copiar o `state.json`, instalar o serviço novo).
*   **Then**: o painel novo mostra os mesmos PRs, nas mesmas fases e com as mesmas adoções. Nenhuma worktree é recriada nem removida no primeiro tick. O daemon novo se recusa a rodar se o LaunchAgent antigo ainda estiver carregado.

### CUJ 9: Reboot do macmini

*   **Given**: o macmini reiniciou e o Rex.app subiu como login item.
*   **When**: o LaunchAgent do daemon volta.
*   **Then**: o daemon volta a sincronizar. As sessões, inclusive a `reviews`, voltam com os mesmos IDs, com um shell no lugar do que rodava antes (spike 7). O autor digita `rrq tui` na sessão `reviews`, e o painel emite de novo os registros.

## Technical Plan (Architecture)

### Ponto de partida

Fork do herdr-reviewq, com histórico, num repo novo, `cirdes/rex-review-queue`. A costura com o Herdr aparece em 11 dos 31 arquivos `.rs` de `src/` (contando `tui/`).

*   **Ficam intactos:** `reconcile`, `github`, `github_extras`, `ci`, `stacks`, `setup`, `facts`, `today`, `requests`, `runner`, `applog`, `git`, `tui/render` (só cores).
*   **Recebem a costura nova:** `executor`, `daemon`, `ui`, `agents`, `state`, `config`, `paths`, `service`, `main`, `tui/run`, `tui/keys`, `tui/model`.

O código específico do Herdr soma ~1,3 mil linhas (sobretudo `herdr.rs`). O resto das ~14,3 mil linhas de `src/` é o núcleo e os testes dele.

### Componentes

```text
 macmini                                                   MacBook
┌──────────────────────────────────────────────────────┐  ┌─────────────────┐
│ LaunchAgent: rex-review-queue daemon                 │  │ Rex.app         │
│  ├─ sync GitHub (gh) → reconcile → executor          │  │ (cliente do     │
│  │     (git worktree + setup + fechamento de sessão) │  │  host macmini)  │
│  └─ state.json  ◄── lê ──┐                           │  │                 │
│                           │                           │  │                 │
│ Servidor Rex (Rex.app como login item)               │  │                 │
│  ├─ sessão "reviews"                                 │  │                 │
│  │   ├─ bloco: rrq tui (aberto pelo autor)           │  │                 │
│  │   │    ├─ OSC 7501 (raiz + 1 registro por PR) ───────► indicador na   │
│  │   │    ├─ Enter/c: session.create, new_split      │  │ aba "reviews"   │
│  │   │    └─ Enter/c: client.perform session.select ────► troca a sessão │
│  └─ sessões "#123 repo" (uma por PR aberto)          │  │                 │
└──────────────────────────────────────────────────────┘  └─────────────────┘
```

Toda a conversa com o Rex passa pelo **CLI `rex`, com uma chamada por operação** (`rex api call <método> <json>`). Quem executa é o `Runner` que já existe, com timeout e saída JSON. No spike, cada chamada custou de 75 a 115 ms no macmini, e o `session.create` local chegou a 204 ms.

### O trait `Herdr` vira dois

1. **`Worktrees`** (git puro, usado pelo executor): cria e remove worktrees. A worktree deixa de ter um workspace atrelado no momento da criação.
2. **`Mux`** (Rex), implementado por `RexCli`, com `FakeMux` nos testes:
   - `list_sessions`, `create_session(label, cwd, command?)`, `destroy_session`, `session_blocks` (via `session.view`);
   - `inspect_block(session, block) -> Process {pid, name, path, argv, cwd}` (via `block call process`, que no spike 7 devolveu `foreground {pid, name, path, cwd}`);
   - `new_split(session, anchor, label, cwd, command, ratio)`, `write(session, block, text)`;
   - `apps() -> [ClientInfo {id, label}]` (via `client.list`, só clientes `kind=app`);
   - `select_session(client, session)` (via `client.perform` com a action `session.select` do app).
   - Erros tipados: `SessionNotFound`, `ClientNotFound`, `BlockNotFound` e `Other`.

### Locks

Cada lock tem um único propósito:

*   **Lock por PR** (`locks/<repo>-<n>.lock`): Enter, combo e fechamento daquele PR. O Enter de um PR nunca espera pelo fechamento de outro. Os arquivos de lock nunca são apagados com processos rodando; o daemon limpa os de PRs que já saíram do estado só na partida.
*   **Lock do `sessions.json`**: só enquanto lê, modifica e renomeia o arquivo. Nunca fica preso durante uma chamada ao `rex`.
*   **Ordem fixa:** lock do PR, depois lock do `sessions.json`.
*   **`panel.lock`:** o TUI que emite os registros 7501 segura esse flock. Um segundo `rrq tui` aberto ao mesmo tempo funciona normalmente, mas não emite status, para que dois blocos não disputem o indicador.

### Sessão de PR: identidade e ciclo de vida

- **Criada sob demanda** pelo painel, no primeiro Enter ou `c`. Label `#123 repo` (`owner/repo#123` com mais de um dono). Uma window com um shell na worktree.
- **Identidade:** o `session_id` fica em `sessions.json` (PR → `{session_id, created_at, entered_at}`). O painel cria entradas e o daemon remove, os dois sob o lock do arquivo. O spike 7 mostrou que o ID sobrevive a restart do servidor, então ele é a identidade primária. O label serve só para leitura humana.
- **App de destino:** o app cujo `label` bate com `rex.app_label` (padrão `macbook`). Sem match, o painel mostra "MacBook não conectado" e não troca a sessão de nenhum outro app (E4, decidido). O `client_id` fica em memória no painel e é refeito quando o Rex responde `ClientNotFound`, porque o ID muda a cada reconexão do app.
- **Enter e `c`:** pegam o lock do PR (espera até 100 ms), relêem o `state.json` e aceitam as mesmas fases que o Enter aceita hoje: qualquer uma menos `Creating` e `Removing`, desde que a worktree exista. Isso inclui `Failed`, `Blocked` e adotados.
  - **Com entrada em `sessions.json`:** `select_session(app, id)`. Com `ClientNotFound` (`code=not_found`), chama `apps()` e repete. Com qualquer outro erro, chama `session.view(id)`: `session_not_found` significa que a sessão sumiu e precisa ser criada de novo como no caso sem entrada; outro resultado é mostrado como erro. O spike mostrou que um `select` numa sessão inexistente volta como `code=internal`, então o `view` é quem decide.
  - **Sem entrada:** `create_session`, grava a entrada e faz o `select`.
  - Grava `entered_at`.

  | Caso | Chamadas | Tempo (spike) |
  | --- | --- | --- |
  | Sessão aberta, `client_id` conhecido | 1 | ~100 ms |
  | Sessão nova, `client_id` conhecido | 2 (`create` + `select`) | ~200–320 ms |
  | Primeiro Enter depois de o painel subir | +1 (`apps`) | +~100 ms |
  | Pior caso (`client_id` velho + sessão fechada à mão) | 5 | ~550 ms |

  **Meta:** o caso típico (1 ou 2 chamadas) fica abaixo de 500 ms. O pior caso pode passar disso e é aceito, porque é raro. O `tui.log` registra a duração de cada Enter.
- **Fechamento** (no `executor::remove`; a fase `Removing` já é gravada hoje pelo `reconcile` depois da carência):
  1. Coleta os facts. Se a worktree divergiu, ela é adotada, a sessão **não** é fechada e o fluxo para.
  2. Com o lock do PR, inspeciona cada bloco da sessão. A sessão está **ociosa** se, em todos os blocos, o processo em foreground for um shell com cwd dentro da worktree. Para cada resposta do Rex:
     - `SessionNotFound`: a sessão não existe mais (fechada à mão ou numa execução anterior). Pula para o passo 4.
     - `BlockNotFound`: o bloco sumiu entre a listagem e a inspeção. Ele é ignorado.
     - outro foreground, ou outro erro: a sessão conta como **em uso**. O PR fica em `Removing` com `leaving_blocked_by_session = true`, e o executor tenta de novo no ciclo de sync seguinte, sem mudar a fase.
  3. `destroy_session`. `SessionNotFound` conta como sucesso.
  4. Apaga a entrada de `sessions.json` e solta o lock do PR.
  5. Descarta a worktree pelas regras de sempre, já fora do lock. Com `Removing` ligado e sem entrada em `sessions.json`, o Enter não cria nada.
  - **Recuperação:** se o processo morrer entre os passos 3 e 5, o ciclo seguinte acha o PR ainda em `Removing`. O passo 2 recebe `SessionNotFound` e segue. O passo 5 trata uma worktree que já não existe como removida. Os testes cobrem essa retomada.
  - O daemon nunca fecha sessão fora de `sessions.json`. O `release` de um PR adotado segue as mesmas regras: fecha a sessão só se estiver ociosa, e senão libera a worktree e mantém a sessão.
  - **Escopo novo:** o Herdr fechava o workspace sem perguntar. Proteger uma sessão em uso é comportamento novo. A regra só olha o foreground, então um job em background num shell não impede o fechamento (ver Riscos).

### Combo (`c`)

Depois do Enter, o painel lê os blocos da sessão e, para `claude` e `reviewr` (achados pelo label):

*   **O bloco não existe:** cria um split com label e comando da config (`combo.claude`, `combo.reviewr`) e cwd na worktree. O `claude` vai à direita do shell. O `reviewr` vai à direita do `claude` e fica com 55% da largura que divide com ele.
*   **O bloco existe, mas o foreground é um shell** (o programa saiu): escreve o comando no bloco e aperta Enter.
*   **O bloco existe e o programa está rodando:** nada a fazer.

O foco termina no `claude`. O combo pode custar até 3 chamadas a mais e não está sujeito à meta de 500 ms. Se falhar no meio, o próximo `c` completa o que faltou.

### Painel

- O autor abre com `rrq tui`. O lugar recomendado é uma sessão `reviews` só para isso, criada uma vez à mão. O daemon não cria, não vigia e não reabre o painel.
- **Trade-off aceito:** com o painel fechado, os avisos (review pronta, falha de setup, erro do daemon) não chegam ao MacBook. O estado continua sendo atualizado, e tudo aparece quando o painel é aberto de novo.
- **Quando o painel cai:** antes de sair, o TUI emite um registro `error` com o motivo, no caminho de erro e num panic hook. Rodando dentro de um shell, o bloco continua e o registro sobrevive (spike 2026-11-27). Saída normal (`q`) emite `clear` na raiz.
- Sai o "roubo de foco" do Herdr e toda a lógica de garantir o painel (`ui.rs:66-180`, `daemon.rs:155-188`).

### Status nativo (OSC 7501)

O painel é o emissor principal. Quando o conjunto de registros muda, ele escreve **só a diferença** no próprio terminal, de forma serializada com o desenho da tela:

- **Raiz:** `app=reviewq`. Fica `error` enquanto houver uma **condição ativa**, e volta a `idle` quando ela some. As condições ativas são funções do estado atual, não do histórico `alerts`: `gh` sem autenticação (`auth_error_notified`), algum repo com o último sync falho (`repos[*].ok == false`). Quando há mais de uma, o `msg` mostra a primeira nessa ordem.
- **Um registro por PR**, `id=pr-<n>-<hash curto do repo>`:
  - `Creating`/`Preparing` → `working`
  - `Ready` sem entrada ainda → `done`, "Review pronta: <título>"
  - `Failed` → `error`, "Setup falhou: <passo>"
  - `Blocked` (o daemon parou de mexer na worktree por um motivo que exige o autor) → `blocked`, `kind=question`
  - entrou, saiu da fila ou foi removido → `clear`
- `title` e `msg` são limpos de caracteres de controle e bidi e truncados nos limites do protocolo (192 e 2048 bytes). Um report que viola o protocolo é descartado inteiro pelo terminal.
- Ao iniciar, o painel manda `clear` geral e emite tudo de novo a partir do estado e do `entered_at`.
- O Rex decide quando apagar um `done`, por exemplo quando o autor aperta uma tecla na aba. Como o autor usa o painel, os `done` podem sumir na primeira tecla. Isso é aceito: o painel continua mostrando o PR como `● pronto`.

### Outros efeitos que mudam de máquina

- **URL (`o`):** o painel já copia a URL por OSC 52 para o clipboard do app (`tui/run.rs:180`). Sai só o `open <url>`, que abriria o navegador do macmini.
- **Notificações do daemon** (`osascript`) deixam de existir. Tudo vira alerta no estado, e o painel transforma o alerta em registro 7501.
- **"Início":** a cada passo de extras (60 s), para cada sessão em `sessions.json` cujo PR ainda não tem `work_started_at`, o daemon inspeciona os blocos. É agente se o nome do executável for exatamente `claude` ou `codex` e o cwd estiver dentro da worktree. A detecção usa o mesmo inspetor do fechamento. Roda no passo de extras do daemon, que tem orçamento de 30 s compartilhado com CI e stacks. CI e stacks vêm primeiro, e o "Início" usa o que sobrar.

### Config, caminhos e serviço

- Saem `herdr_session`, `backend`, `window` e `[notify]`. Entram `[rex]` (`bin`, padrão `~/.local/bin/rex`, e `app_label`, padrão `macbook`) e `[combo]` (`claude` e `reviewr`, cada um um argv em que `{worktree}` é substituído; padrões `["claude"]` e `["reviewr", "{worktree}"]`). O `~` inicial é expandido na leitura da config. O `rex.bin` tem caminho absoluto porque o LaunchAgent não herda o PATH do shell.
- Diretórios `~/.config/rrq/` e `~/.local/state/rrq/`. Binário `~/.local/bin/rrq`. LaunchAgent `com.cirdes.rrq`. O PATH do plist continua o mesmo de hoje (inclui os shims do mise).
- **Update explícito:** cada push na `main` dispara o workflow de build, que publica uma release rolante `main` com `rrq-<target>.tar.gz`, `.sha256` e um `version.txt` com o SHA do commit. O `rrq` grava o SHA do próprio build (via `build.rs`). `rrq update` compara o SHA local com o `version.txt` da release `main`; se forem diferentes, baixa o tarball do Mac certo, confere o checksum, troca o binário de forma atômica e reinicia o daemon. `rrq update --check` só informa. Uma vez por dia, no passo de extras, o daemon faz a mesma comparação e o rodapé do painel mostra quando há build nova. Não existe update automático. Um painel aberto continua no binário antigo até o autor sair e digitar `rrq tui` de novo, e o rodapé avisa isso.
- Sai o manifesto de plugin. Quem abre o painel é o daemon.

### Migração

Não há subcomando de importação. O formato do `state.json` é compatível nos dois sentidos: campos desconhecidos são ignorados e os campos que saem do uso continuam no struct com `serde(default)`. Passos, no README:

1. `herdr-reviewq service uninstall`
2. copiar o `state.json` para `~/.local/state/rrq/`
3. copiar o `config.toml` para `~/.config/rrq/`, tirar `herdr_session`, `backend`, `window` e `[notify]`, e acrescentar `[rex]` e `[combo]` se os padrões não servirem (o `rrq doctor` aponta pelo nome qualquer chave antiga que tenha sobrado)
4. `rrq start`
5. `rrq doctor`

Um PR cujo `path` não existir é mantido e tratado como hoje (worktree ausente). O daemon novo e o `doctor` **recusam rodar** enquanto o LaunchAgent `dev.cirdes.herdr-reviewq` estiver carregado, então os dois nunca operam juntos sobre o mesmo `worktrees_dir`.

**Rollback:** o inverso, copiando de volta o `state.json` **atual**, e não um snapshot anterior. Assim as worktrees criadas ou adotadas depois do corte não se perdem.

## Alternatives Considered

### Alternativa A: adicionar um backend Rex ao herdr-reviewq

*   **Prós**: código e histórico num lugar só. O trait `Herdr` e a seleção `auto` já preveem vários backends.
*   **Contras**: dois multiplexadores no mesmo binário dobram os caminhos de teste, e toda mudança feita para o Rex teria de manter o Herdr funcionando. O trait tem o formato do Herdr (worktree nativa, pane de plugin, metadata de workspace), e o Rex não tem nada disso.
*   **Motivo da rejeição**: Rejeitada pelo autor. Como o Herdr vai ser abandonado, manter dois backends e uma abstração com o formato do Herdr não traz benefício.

### Alternativa B: reescrever do zero

*   **Prós**: arquitetura livre do formato do herdr-reviewq, por exemplo um daemon guiado por eventos do Rex.
*   **Contras**: o código específico do Herdr é menos de 10% do total. O núcleo, de ~10 mil linhas, tem comportamento ajustado e testado (`reset --keep`, carência, adoção, `disposable_ignored`).
*   **Motivo da rejeição**: Rejeitada pelo autor. Reescrever arrisca perder as garantias sobre worktrees, e uma arquitetura diferente não compensa esse risco.

### Alternativa C: crate compartilhado entre herdr-reviewq e rex-review-queue

*   **Prós**: uma correção no núcleo valeria para os dois.
*   **Contras**: exige manter uma API estável entre dois repos que têm um único usuário.
*   **Motivo da rejeição**: Rejeitada pelo autor. O herdr-reviewq fica congelado, e as correções, se houver, vão por cherry-pick.

### Alternativa D: conexão persistente com a API de controle do Rex

*   **Prós**: chamadas de poucos ms e eventos sem polling.
*   **Contras**: exige implementar em Rust o transporte do Rex (handshake, streams, reconexão). Esse transporte não está documentado além do schema dos métodos e pode mudar entre previews.
*   **Motivo da rejeição**: Rejeitada pelo autor. A CLI cumpre a meta de 500 ms (75 a 115 ms por chamada no spike) e isola o projeto de mudanças no transporte.

### Alternativa E: cache de sessões em memória no painel

*   **Prós**: o Enter nunca precisaria ler o `sessions.json`.
*   **Contras**: o cache pode ficar velho (sessão fechada à mão ou pelo daemon) e exige atualização em segundo plano no TUI. O `sessions.json` já dá o caminho rápido de 1 chamada. Só o `client_id` fica em memória, porque é refeito com uma chamada quando falha.
*   **Motivo da rejeição**: Rejeitada pelo autor. O cache de sessões duplicaria o estado que já está no `sessions.json`.

### Alternativa F: uma sessão por PR assim que a worktree fica pronta

*   **Prós**: cada review pronta teria seu próprio indicador na sidebar.
*   **Contras**: de 10 a 20 sessões na sidebar para PRs que talvez nunca sejam abertos, e mais sessões para o daemon fechar depois.
*   **Motivo da rejeição**: Rejeitada pelo autor. Para o Enter ser rápido basta a worktree estar pronta; a sessão não precisa existir antes.

### Alternativa G: emitir o OSC 7501 de dentro da sessão do PR (setup rodando num bloco)

*   **Prós**: o status ficaria na sessão certa, e o setup apareceria ao vivo.
*   **Contras**: depende da Alternativa F, que foi rejeitada, e exige redesenhar o executor (redação de log, timeout, retorno do resultado ao daemon). Depois que o setup termina, nenhum processo atualiza o status quando chegam commits novos.
*   **Motivo da rejeição**: Rejeitada pelo autor. Com sessão sob demanda, só o painel está vivo antes do Enter.

### Alternativa H: achar a sessão pelo caminho da worktree, com o ID só como cache

*   **Prós**: sobreviveria à perda do `sessions.json`.
*   **Contras**: achar a sessão pelo caminho exige listar sessões e inspecionar cada bloco (~100 ms por bloco), o que estoura a meta do Enter quando há várias sessões abertas.
*   **Motivo da rejeição**: Rejeitada pelo autor. O spike 7 mostrou que o ID sobrevive a restart do servidor. A perda do arquivo está tratada em Riscos.

### Alternativa I: trocar o painel pela command palette do Rex (`rex.ui.pick`)

*   **Prós**: lista nativa, com filtro e ícones, sem um processo TUI para manter vivo.
*   **Contras**: sem colunas, cor, atualização ao vivo, contadores, stacks nem CI detalhado. Cada item comporta só um texto curto à direita.
*   **Motivo da rejeição**: Rejeitada pelo autor. O painel depende dessas colunas e contadores, e a palette não tem como mostrá-los.

### Alternativa J: Shift+Enter para o combo

*   **Prós**: tecla natural, ao lado do Enter.
*   **Contras**: só se distingue de Enter com o protocolo de teclado estendido (kitty), e o Rex captura combinações com `cmd` antes do TUI.
*   **Motivo da rejeição**: Rejeitada pelo autor. Depender do protocolo de teclado é um risco que uma tecla simples (`c`) evita.

### Alternativa K: bridge com polling para o foco remoto (como o `reviewq-bridge` do herdr-app)

*   **Prós**: já existe e funciona no Herdr.
*   **Contras**: exige um mod JS no app e um documento intermediário consultado a cada 1 s.
*   **Motivo da rejeição**: Rejeitada pelo autor. No spike, `client.perform session.select` trocou a tela do MacBook a partir do macmini em ~100 ms, sem intermediário.

### Alternativa L: servidor Rex headless no macmini por LaunchAgent próprio (`rex server run`)

*   **Prós**: não depende de sessão gráfica nem de auto-login.
*   **Contras**: não é suportado no macOS (`rex server install` só cobre Linux e Windows). Haveria dois servidores disputando o mesmo socket quando o Rex.app abrir o dele.
*   **Motivo da rejeição**: Rejeitada pelo autor. O caminho suportado é o Rex.app como login item.

### Alternativa M: abrir URLs no navegador do MacBook com `client.url.open`

*   **Prós**: o `o` abriria o PR direto no MacBook, em vez de só copiar a URL.
*   **Contras**: a action só existe no canal Bleeding Edge do app.
*   **Motivo da rejeição**: Rejeitada pelo autor. Os fluxos centrais não dependem de recursos do Bleeding Edge, e o OSC 52, que já existe, cobre o caso.

--------------------------------------------------------------------------------

## Riscos e Mitigações

| Risco | Impacto | Mitigação |
| --- | --- | --- |
| `sessions.json` perdido ou corrompido | Sessões órfãs que o daemon não fecha. O Enter cria uma sessão nova. | Tratado como o `state.json` corrompido hoje: o arquivo é renomeado, o mapa é recriado vazio e um alerta é emitido. O `doctor` lista sessões `#… repo` fora do mapa para o autor fechar. |
| Servidor Rex fora do ar ou CLI com timeout | Enter falha. O painel e os avisos somem. | Timeout de 5 s por chamada. O Enter mostra o erro no rodapé. O daemon segue sincronizando worktrees. O `doctor` diagnostica. |
| Painel fechado ou caído | Avisos não chegam ao MacBook. | Aceito: o autor abre com `rrq tui`. Ao cair, o TUI deixa um `error` no bloco (CUJ 7). |
| Combo falha no meio | Sessão com só um dos dois programas. | O combo é idempotente: o próximo `c` completa. |
| Job em background num shell | O fechamento mata o job (regra só olha o foreground). | Aceito. É o mesmo comportamento do Herdr, que fechava sem perguntar. |
| Sessão em uso por muito tempo | A worktree não é removida. | O painel mostra `Saindo · sessão em uso` com o tempo retido. Fechar o programa, ou a sessão, libera a remoção no ciclo seguinte. |
| Custo de inspeção por tick | Com muitas sessões, o "Início" não termina dentro do orçamento. | Só inspeciona PRs sem `work_started_at`. CI e stacks têm prioridade. |
| Mudanças do Rex (preview) | Payload ou método muda numa atualização. | API revision mínima checada na partida, fixtures da revision 24 e o `doctor`. |
| Código de PR controla o app do MacBook | Um script de setup malicioso abre URLs ou blocos no MacBook. | Risco aceito pelo autor (E1). |
| Auto-login do macmini | Acesso físico dá sessão aberta. | Pendente de decisão do autor (E2). |

## Open Questions

*   **E1. Modelo de ameaça (decidido: risco aceito).** O setup roda scripts de PRs com o mesmo usuário do servidor Rex e, por isso, alcança o app do MacBook via `client.perform`. O autor aceita o risco: os PRs são de colegas e os de forks são ignorados. O `setup.rs` não muda.
*   **E2. Auto-login do macmini.** Sem auto-login, o servidor Rex não volta depois de um reboot. Com auto-login, quem tiver acesso físico tem uma sessão aberta com `gh` e as chaves ssh, e o FileVault não pode desbloquear no boot. **Decisão do autor.**
*   **E5. Linha de base do tempo até a worktree pronta.** Proposta: medir a mediana dos últimos 30 dias no `daemon.log` do herdr-reviewq antes da troca e registrar no README. **A confirmar.**
*   **Activity e notificação:** confirmar no primeiro uso se a Activity (`cmd+.`) mostra o `msg` de cada PR e se o macOS notifica. Não muda a arquitetura.
*   **LaunchAgent e socket:** supõe-se que o daemon acha o servidor em `~/Library/Application Support/rex/server.sock`, usando o `rex` em `~/.local/bin`. Validar no primeiro `service install`.

## Non-Functional Requirements

*   **Latência:** ver a tabela do Enter. O `tui.log` registra cada Enter para medir no uso real.
*   **Segurança:** o daemon nunca fecha sessão que ele não criou, nem sessão em uso. As garantias sobre worktrees não mudam. **Regra:** os comandos de `[combo]` na config não podem conter segredos, porque vão para o argv do `rex` e para o processo do bloco.
*   **Rollout e rollback:** ver Migração.
*   **Dependência do Rex em preview:** API revision mínima 24, a dos spikes. Abaixo dela, o daemon e o `doctor` recusam rodar com uma mensagem que diz a revision encontrada, a mínima exigida e como atualizar o Rex.

## Additional Resources

*   [spike_results.md](spike_results.md): medições do Estágio 0. No repo, ele vai para `docs/designs/`.
*   herdr-reviewq: `~/Workspaces/herdr-reviewq` (README e `src/`).
*   [Docs do Rex](https://www.superlogical.com/rex/docs/llms.txt), em especial Program Status Protocol, Custom UI, Lua Events e Server API.
*   Config Rex do autor: `~/Workspaces/dotfiles/rex/`. O `reviewr.lua` usa `session.new_split` em produção e mostra o layout do combo.

## Future Improvements

*   **Picker na palette** (`rex.ui.pick`), como atalho para entrar numa review sem passar pelo painel.
*   **Tecla global** no `init.lua` para ir ao painel, carregada com uma guarda para só existir onde o daemon roda.
*   **"Início" por eventos** (`rex events --json` como subprocesso), em vez de inspecionar a cada passo de extras.
*   **OSC 7577** no painel e no Claude, para que voltem sozinhos depois de um restore.
*   **Conexão persistente com a API** (Alternativa D), se a latência da CLI incomodar no uso real.
*   **Retenção mais ativa:** um aviso `blocked` depois de N horas de sessão em uso, e Enter permitido em `Removing` para voltar à sessão retida pelo painel. Na primeira entrega, a sessão retida é aberta pela sidebar do Rex.

--------------------------------------------------------------------------------

## Detailed Implementation Plan

### Preparação do repo

A pasta `~/Workspaces/rex-review-queue` já tem os documentos de design, então `git clone` não funciona. Os passos são:

```sh
cd ~/Workspaces/rex-review-queue
git init -b main
git remote add herdr https://github.com/cirdes/herdr-reviewq.git
git fetch herdr main
git reset --hard herdr/main      # traz o código; os arquivos locais não rastreados ficam
cat .gitignore.local >> .gitignore   # ver item 30
gh repo create cirdes/rex-review-queue --private --source . --remote origin
```

O `.gitignore` local atual é renomeado para `.gitignore.local` antes do `reset` e mesclado depois (item 30). O remote `herdr` é removido depois da primeira fase verde. Os caminhos abaixo são relativos à raiz do repo.

### Premissas do Rex (já medidas)

Medidas no mini-spike de 2026-11-27 (`spike_results.md`). Viram fixtures na fase 2:
- `server.version` traz `api_revision`;
- o layout aceita `block.label`;
- o painel roda dentro de um shell via `initial_input` e sobrevive à saída do TUI;
- `process` traz `name`, `path`, `argv0` e `cwd`, sem argv completo;
- `client.perform` com cliente inexistente volta `code=not_found`, e um `select` numa sessão inexistente volta `code=internal`;
- `session.view` com sessão inexistente volta `code=session_not_found`;
- bloco inexistente volta exit 3 com "block target … not found";
- `REX_BLOCK` está no ambiente de todo bloco;
- `session.focus_block` e `session.set_block_label` existem.

### Fases

Cada fase termina com `cargo build`, `cargo test` e `cargo clippy --all-targets -- -D warnings` verdes, e com um commit.

| Fase | Entrega | Itens |
| --- | --- | --- |
| 1 | Nome `rrq`, caminhos novos e `Worktrees`. O executor deixa de criar worktree pelo Herdr, mas o `herdr.rs` continua no lugar. | 7, 14 (só o rename), 16, 17, 26, 28 |
| 2 | `Mux`, `RexCli`, fixtures e `FakeMux`. | 5, 6, 12 |
| 3 | `sessions`, `locks`, `pst`. | 8, 9, 10 |
| 4 | Enter, `c`, `f` e o TUI no Rex. | 11, 21, 23, 24, 25 |
| 5 | Daemon e executor no Rex: fechamento de sessão, "Início", partida, update. | 15, 18, 19, 20, 22, 27 |
| 6 | Remoção do Herdr, config, comandos novos, README e release. | 1, 2, 3, 4, 13, 14 (resto), 29, 30, 31, 32 |

### Target Files to Create/Modify

#### Removidos (fase 6)

1.  **`src/herdr.rs`** (Remover). Antes, `is_tui_argv` (147-155) e `AgentInfo` (39-44) vão para `agents.rs` ou `ui.rs`, e o `git worktree add` (716-720) vai para `worktrees.rs`.
2.  **`herdr-plugin.toml`, `herdr/install.sh`, `bin/herdr-reviewq`** (Remover).
3.  **`herdr-app/mods/reviewq-bridge/main.js`, `herdr-app/mods/reviewq-bridge/mod.json`** (Remover).
4.  **`docs/superpowers/plans/2026-10-13-sugestoes.md`, `docs/superpowers/specs/2026-10-13-sugestoes-design.md`** (Remover). Continuam no histórico do git.

#### Criados

5.  **`src/mux.rs`** (Criar, fase 2).
    *   Trait `Mux`:
        *   `server_api_revision() -> u32`
        *   `list_sessions() -> Vec<SessionInfo {id, label}>`
        *   `create_session(CreateReq {label, cwd, block_label, initial_input: Option<String>}) -> Created {session_id, block_id}`
        *   `view(session) -> Vec<BlockInfo {id, label}>`
        *   `destroy_session(session)`
        *   `inspect_block(session, block) -> Option<Process {pid, name, path, argv0, cwd}>` (`None` quando o bloco não tem foreground)
        *   `new_split(SplitReq {session, anchor, side: Right, ratio, label, cwd, initial_input}) -> block_id`
        *   `write(session, block, text)`
        *   `focus_block(session, block)`
        *   `apps() -> Vec<ClientInfo {id, label}>`
        *   `select_session(client, session)`
    *   `MuxError::{Unreachable, ClientNotFound, SessionNotFound, BlockNotFound, Other(String)}`.
    *   `inspect_session(mux, session) -> Result<Vec<(BlockInfo, Option<Process>)>>`: ignora `BlockNotFound`.
    *   `is_shell(p) -> bool`: `name` em {`zsh`, `bash`, `fish`, `sh`}, ou `argv0` igual a um desses com `-` na frente.
    *   `in_worktree(p, wt) -> bool`: compara caminhos canonicalizados.
    *   `FakeMux` (em `testutil`): sessões, blocos e processos em memória; registra as chamadas; permite injetar cada `MuxError` por método.
6.  **`src/rex.rs`** (Criar, fase 2). `RexCli` implementa `Mux` executando `<rex.bin> api call <método> <json>` e `<rex.bin> block call -s <s> -b <b> process|write` pelo `Runner`, com timeout de 5 s por chamada.
    *   Os payloads são funções puras: `create_args`, `split_args`, `perform_select_args`. Nos blocos, o comando sempre vai em `initial_input`; o `command` não é usado.
    *   O erro é mapeado assim:
        *   falha de conexão (stderr com "connecting to Rex server"): `Unreachable`;
        *   `code=not_found` no `client.perform`: `ClientNotFound`;
        *   `code=session_not_found`: `SessionNotFound`;
        *   "block target … not found": `BlockNotFound`;
        *   qualquer outro: `Other`.
    *   O parse do JSON é tolerante: ignora campos desconhecidos e devolve `Other` quando falta um campo obrigatório.
    *   Testes: cada payload e cada mapeamento de erro contra as fixtures.
7.  **`src/worktrees.rs`** (Criar, fase 1). Trait `Worktrees` com `create(repo, branch, base, path)` (`git worktree add -q -b <branch> <path> <base>`) e `remove(repo, path)` (`git worktree remove` sem `--force`, seguido de `prune`). Uma worktree que não existe mais conta como removida. A implementação é `GitWorktrees` sobre o `Git`. Os testes usam o `GitWorktrees` real em repositórios temporários, como o `FakeHerdr` já fazia (`herdr.rs:966-987`). Não existe `FakeWorktrees`. O `git.worktree_prune` em `executor.rs:214` e `261` sai, porque o `remove` já faz o prune.
8.  **`src/sessions.rs`** (Criar, fase 3). Arquivo `sessions.json` em `state_dir`, com `BTreeMap<String /* key.to_string() */, SessionEntry {session_id: String, shell_block: String, created_at: DateTime<Utc>, entered_at: Option<DateTime<Utc>>}>`.
    *   Funções: `load`, `get`, `list`, `upsert`, `remove`, `mark_entered`.
    *   A escrita é atômica (tmp + rename) e feita sob o `sessions.lock`, que só fica preso durante essa operação.
    *   Arquivo corrompido: o daemon renomeia para `.corrupt-<ts>`, recomeça vazio e grava `sessions_corrupt_since` no estado (condição ativa da raiz, que some no próximo `upsert` bem-sucedido). O TUI trata o arquivo como vazio e mostra o erro no rodapé, sem renomear.
    *   Testes: ida e volta, escrita atômica, arquivo corrompido no daemon e no TUI.
9.  **`src/locks.rs`** (Criar, fase 3). Funções sobre o flock com espera limitada (o atual `ui::lock`, com a mensagem de erro parametrizada):
    *   `pr_lock(key, wait)`, com o nome de arquivo `pr-<repo com / trocado por ->-<n>-<hash8 de key>.lock`;
    *   `sessions_lock()`;
    *   `try_panel_emitter()` para o TUI: tenta pegar o `panel.lock` sem esperar. Se conseguir, aquele TUI é o emissor do 7501. Se não, roda sem emitir status.

    Os arquivos ficam em `state_dir/locks/` e nunca são apagados. Ordem de aquisição: PR, depois sessions. Testes: ordem e um único emissor com dois TUIs.
10. **`src/pst.rs`** (Criar, fase 3). Program Status Protocol.
    *   `report(id, state, kind, app, title, msg)` e `clear(id)`. `sanitize` remove controles C0/C1 e bidi e trunca `title` em 192 bytes e `msg` em 2048, em fronteira UTF-8.
    *   O base64 é o `base64_lite`, que sai de `tui/run.rs:186` e passa a ser exportado daqui.
    *   `active_conditions(state) -> Vec<String>`, na ordem: gh sem auth, repo sem sync, `sessions_corrupt_since`.
    *   `records_for(state, sessions)`, com precedência por PR:
        1. sem worktree ou fora do estado: `clear`;
        2. `entered_at` presente: `clear`;
        3. `Failed`: `error`;
        4. `Blocked`: `blocked`, `kind=question`;
        5. `Creating`/`Preparing`: `working`;
        6. `Ready`: `done`;
        7. `Removing`: `clear`.
    *   `diff(old, new)`; `exit_error(msg)`.
    *   Testes byte a byte, de limites, de precedência, de diff e de condições ativas.
11. **`src/focus.rs`** (Criar, fase 4). Enter e combo, testáveis com `FakeMux`.
    *   `eligible(rec) -> bool`: o worktree existe e a fase não é `Creating` nem `Removing`. É a mesma regra de `ui.rs:168-181`, usada pelo Enter e pelo `c`.
    *   `enter(ctx, key) -> Result<EnterOutcome>`:
        *   pega `pr_lock(key, 100 ms)`; sem o lock, devolve `Busy`;
        *   relê o estado e confere `eligible`;
        *   sem `client_id` em cache, chama `apps()` e escolhe o que tem `label == rex.app_label`; sem match, devolve `NoApp` ("MacBook não conectado");
        *   com entrada no `sessions.json`, chama `select`. Com `ClientNotFound`, chama `apps()` e repete uma vez. Com outro erro, chama `view(id)`: se vier `SessionNotFound`, segue o caminho "sem entrada"; se vier qualquer outra coisa, devolve o erro;
        *   sem entrada, chama `create_session(label "#<n> <repo>", cwd = worktree, block_label "shell")`, faz `upsert` com `shell_block` e chama `select`;
        *   por fim, `mark_entered` e grava a duração no `tui.log` como `enter <key> <ms>ms <n> chamadas`.
    *   `combo(ctx, key)`:
        *   roda o `enter` e depois o `inspect_session`;
        *   bloco `claude` ausente: `new_split(anchor = shell_block, Right, ratio 0.5, label "claude", initial_input = argv do combo.claude + "\r")`;
        *   bloco existe com shell em foreground: `write` do comando;
        *   bloco existe com o programa rodando: nada;
        *   o `reviewr` segue a mesma regra, com âncora no bloco `claude`, `Right`, ratio 0.45. Esse ratio dá 45% ao `claude` e 55% ao `reviewr` da largura que os dois dividem;
        *   no fim, `focus_block(claude)`.

        O argv vira texto com quoting POSIX (`'…'`, trocando cada `'` interna por `'\''`) e `{worktree}` é substituído.
    *   Testes:
        *   `select` direto;
        *   `ClientNotFound` → `apps` e repete;
        *   `select` com `Other` → `view` → `SessionNotFound` → cria;
        *   `NoApp`;
        *   `Busy`;
        *   fase não elegível;
        *   combo nos três estados de bloco;
        *   combo repetido sem duplicar;
        *   combo que falha no meio e é completado no `c` seguinte;
        *   quoting com espaços e aspas.
12. **`tests/fixtures/rex/`** (Criar, fase 2). Fixtures capturadas da revision 24:
    *   `server_version.json`, `session_create.json`, `session_view.json`, `session_list.json`, `new_split.json`, `client_list.json`, `perform_ok.json`;
    *   `block_process_running.json`, `block_process_shell.json`, `block_process_exited.json`, `block_write.json`;
    *   `err_client_not_found.txt`, `err_select_missing_session.txt`, `err_view_session_not_found.txt`, `err_block_not_found.txt`, `err_unreachable.txt`.

#### Modificados

13. **`src/lib.rs`** (Modificar, fase 6). Sai `herdr`. Entram `mux`, `rex`, `worktrees`, `sessions`, `locks`, `pst`, `focus` e `update`.
14. **`src/main.rs`** (Modificar). `#[command(name = "rrq")]`, e todos os `use herdr_reviewq::` viram `use rrq::` (fase 1). Subcomandos (fase 6):
    *   `start`: o atual `service install`.
    *   `stop`: o atual `service uninstall`.
    *   `restart`.
    *   `status`: o atual `status`, mais o estado do LaunchAgent e a linha "sessão em uso".
    *   `tui`.
    *   `doctor`: confere o `rex`, a revision, a conexão, o app com o label configurado, `combo.*` no PATH do LaunchAgent, o LaunchAgent antigo carregado, chaves antigas na config e sessões `#<n> <repo>` fora do `sessions.json`.
    *   `update [--check]`.
    *   Ocultos na ajuda: `daemon` e `request`.

    Saem `service`, `ui ensure/open` e `focus first-ready`.
15. **`src/config.rs`** (Modificar, fase 5). Saem `herdr_session`, `backend`, `window`, `Backend`, `Window` e `NotifyConfig`. Entram `RexConfig {bin: PathBuf = "~/.local/bin/rex", app_label: String = "macbook"}` e `ComboConfig {claude: Vec<String> = ["claude"], reviewr: Vec<String> = ["reviewr", "{worktree}"]}`. A leitura expande `~` no início dos caminhos. Uma chave antiga gera um erro que cita a chave e o README. Testes: padrões, expansão de `~`, chave antiga com mensagem.
16. **`src/paths.rs`** (Modificar, fase 1). Diretórios `~/.config/rrq/` e `~/.local/state/rrq/`. Novos `sessions_file()` e `locks_dir()`. Sai `ui_lock_file()`.
17. **`src/service.rs`** (Modificar, fase 1). `LABEL = "com.cirdes.rrq"`, binário `~/.local/bin/rrq`. O `path_env` não muda. `is_loaded` passa a receber o label, e `const OLD_AGENT_LABEL = "dev.cirdes.herdr-reviewq"` existe só aqui, usada por `old_agent_loaded()`. Saem os testes que leem `herdr-plugin.toml`, `bin/` e `herdr/install.sh`.
18. **`src/executor.rs`** (Modificar, fase 5).
    *   `Ctx` passa a ser `{cfg, runner, worktrees, mux, paths, now}`, e `Ctx.herdr` sai.
    *   A criação chama `worktrees.create`. `workspace_id` não é mais gravado.
    *   **`remove`** (191-224), na ordem:
        1. coleta os facts; se divergiram, adota e para (como hoje);
        2. `close_session(ctx, key) -> Closed | NoSession | InUse`. Sem entrada em `sessions.json`, devolve `NoSession`. Com entrada, tenta `pr_lock(key, 0)`; se não conseguir, devolve `InUse`. Depois roda `inspect_session`:
           *   `SessionNotFound`: `NoSession`;
           *   algum bloco com foreground que não é shell, ou fora da worktree, ou `Other`/`Unreachable`: `InUse`;
           *   todos os blocos com shell na worktree: `destroy_session` (`SessionNotFound` conta como sucesso), `sessions.remove`, e devolve `Closed`;
        3. com `InUse`: liga `leaving_blocked_by_session`, grava `blocked_since` se ainda não tiver e devolve `Ok(Some("sessão em uso"))`;
        4. **coleta os facts de novo**; se divergiram, adota e para. Isso protege arquivos ignorados criados enquanto a sessão era fechada;
        5. backup do head, `discard_worktree` (que passa a chamar só `worktrees.remove`), `delete_owned_branch`.
    *   **`release`** (227+): sai `find_open_workspace` (234). Usa `close_session`. Com `InUse`, libera a worktree mantendo a sessão e remove a entrada do `sessions.json`.
    *   A notificação "#N removido" (220-221) sai.
    *   Testes:
        *   saem `remove_never_reopens_a_closed_workspace` (679-712) e as verificações de notificação;
        *   os demais migram para `GitWorktrees` real + `FakeMux`;
        *   regressões novas: worktree suja → adotada com sessão mantida; branch alterada → preservada; Claude em foreground → `InUse` sem remoção; `Other` no inspect → `InUse`; `SessionNotFound` no inspect → remove; `BlockNotFound` → bloco ignorado; lock ocupado → `InUse`; arquivo ignorado criado entre a primeira e a segunda coleta de facts → adotada; retomada depois de morrer entre `destroy` e `remove`; `release` com sessão em uso; sessão fora do `sessions.json` nunca é fechada.
19. **`src/daemon.rs`** (Modificar, fase 5). Recebe `&dyn Mux` e `&dyn Worktrees`.
    *   **Partida** (876-908): revision abaixo de 24 ou `old_agent_loaded()` faz o daemon sair com erro. `Unreachable` não impede a partida: o daemon segue sincronizando e trata o Rex como ausente (sem fechar sessões nem detectar "Início") até conseguir falar com ele.
    *   **Painel:** sai tudo. Isso inclui `maybe_ensure_panel`, `ensure_panel_now`, `panel_ensured`, `panel_next_try`, `panel_backoff`, `panel_failures`, `panel_alerted` e `ui_herdr` (`daemon.rs:73-80`, `150-188`).
    *   **Saem:**
        *   `report_metadata` (714-715);
        *   `maybe_check_window`, `last_window_check`, `window_alerted` e `WINDOW_CHECK_EVERY` (85-91, 242-260);
        *   `deliver_notifications`, `notify_backoff` e `last_notify_try` (677, 693-724);
        *   a notificação de gh sem auth (286). A condição continua em `auth_error_notified`.
    *   **Extras** (453+): CI e stacks primeiro, depois "Início" com `agents::scan` no orçamento que sobrar. Uma vez por dia, `update::check` grava `update_available` no estado.
    *   **Testes:** migram para os fakes. Saem os testes de painel (`daemon.rs:1622-1680`). Casos novos: `Unreachable` na partida → o sync continua; revision 23 → sai; LaunchAgent antigo carregado → sai.
20. **`src/ui.rs`** (Remover, fase 5). Saem `ensure`, `give_focus_back`, `open`, `focus_first_ready`, `focus_pr`, `notify_error` e os testes deles. O `lock` vai para `locks.rs`, `pick_first_ready` vai para `focus.rs` e `remaining` sai.
21. **`src/agents.rs`** (Modificar, fase 4). `AgentInfo` passa a morar aqui. `scan(mux, sessions, state)`: só olha PRs sem `work_started_at`. É agente quando o `name`, o basename de `path` ou o `argv0` é exatamente `claude` ou `codex`, e o `cwd` está dentro da worktree. `apply_agents` não muda. Testes: `vim claude.md` não marca; PR já marcado não é inspecionado.
22. **`src/state.rs`** e **`src/reconcile.rs`** (Modificar, fase 5).
    *   Entram `leaving_blocked_by_session: bool`, `blocked_since: Option<DateTime<Utc>>`, `sessions_corrupt_since` e `update_available: Option<String>`, todos com `serde(default)`.
    *   `workspace_id`, `notify_pending` e os campos de notificação **ficam** no struct com `serde(default)`, para o rollback, mas o código deixa de escrevê-los. `reconcile.rs:198` e `205` param de preenchê-los.
    *   O `reconcile` zera `leaving_blocked_by_session` e `blocked_since` quando um PR sai de `Removing` (115-120).
    *   Testes:
        *   `tests/fixtures/herdr_state.json` (um `state.json` real do herdr-reviewq, sanitizado) é lido;
        *   um estado gravado pelo `rrq` é lido por uma cópia congelada mínima do `PrRecord` antigo em `tests/compat/herdr_state.rs`, com os campos obrigatórios do herdr-reviewq;
        *   flags zerados quando o PR volta à fila.
23. **`src/tui/run.rs`** (Modificar, fase 4).
    *   Na partida, chama `try_panel_emitter()`. Sem o lock, roda normalmente, sem emitir 7501, e mostra "outro painel emite os avisos" no rodapé.
    *   Instala um panic hook que emite `pst::exit_error`. Saída por erro também emite `exit_error`. Saída normal (`q`, `Esc` sem busca, `Ctrl-C`) emite `clear` na raiz.
    *   O `WorkerCtx` ganha o `RexCli` e o cache do `client_id`. O Enter de review (243) chama `focus::enter`, e as ações `Combo` e `FirstReady` são novas. O worker devolve um evento `Entered(key)`, e o loop relê o `sessions.json` a cada evento e por mtime, como o `StateWatch` (399-423).
    *   O 7501 é emitido no mesmo ponto em que a tela é desenhada, com `pst::diff`.
    *   O split do log (273-312, 374-385) passa a usar `mux.new_split(anchor = REX_BLOCK, label "log", initial_input "less +F <log>\r")`. A sessão vem de `session.view` do bloco atual.
    *   `OpenUrl` (261-267) perde o `open <url>` e mantém o OSC 52.
    *   O rodapé mostra `update_available` e avisa quando o binário em disco é mais novo do que o painel aberto ("saia e rode `rrq tui` de novo").
24. **`src/tui/keys.rs`** (Modificar, fase 4). Entram só `c` (`Combo`, com `eligible.combo`) e `f` (`FirstReady`). As teclas atuais continuam como estão: setas/`k`/`j`, Enter, `o`, `l`, `s`, `R` (tentar de novo), `a`, `r`, `/`, `Esc` (limpa a busca ou sai), `q` e `Ctrl-C` (saem). Testes para `c` e `f`.
25. **`src/tui/model.rs`** (Modificar, fase 4). `eligible.focus` e o novo `eligible.combo` usam `focus::eligible`. Novo estado `Saindo · sessão em uso há <tempo>` quando `leaving_blocked_by_session`. O rodapé mostra `c combo` e `f próxima`.
26. **`Cargo.toml`** (Modificar, fase 1). `name = "rrq"`, `version = "0.1.0"`, sem dependências novas. **`Cargo.lock`** regenerado.
27. **`src/update.rs`** e **`build.rs`** (Criar, fase 5).
    *   O `build.rs` grava o SHA do commit (`git rev-parse HEAD`, ou `GITHUB_SHA` no CI) em `env!("RRQ_BUILD_SHA")`. `rrq --version` mostra a versão e o SHA.
    *   `check(runner) -> Option<String>`: `gh release download main --repo cirdes/rex-review-queue --pattern version.txt` num diretório temporário. Devolve o SHA remoto se for diferente do local.
    *   `install(runner)`:
        1. com `check` positivo, baixa `rrq-<target>.tar.gz` e o `.sha256` da release `main` e confere o checksum;
        2. extrai para `~/.local/bin/rrq.new` e faz o rename atômico;
        3. `restart` do daemon;
        4. imprime "painel aberto continua na versão antiga; saia e rode `rrq tui`".
    *   Testes com `Runner` fake: SHA igual → nada; checksum errado aborta sem trocar; sucesso troca o binário e reinicia.
28. **`config.example.toml`** (Modificar, fase 1). Sem chaves do Herdr. Com `[rex]` e `[combo]` comentados e os padrões.
29. **`README.md`** (Modificar, fase 6).
    *   Requisitos: Rex.app no macmini como login item, MacBook conectado ao host `macmini`, `gh` autenticado, reviewr para Rex em `~/.local/bin`.
    *   Instalação: baixar o tarball da release `main` (ou `cargo build --release` e copiar para `~/.local/bin/rrq`), `rrq start`, `rrq doctor`. Abrir o painel: criar uma sessão `reviews` e rodar `rrq tui`.
    *   Comandos.
    *   Migração (passos do Technical Plan) e rollback.
    *   Medição da linha de base (E5).
    *   Painel: seções, colunas e teclas.
    *   Status nativo.
    *   Sessão em uso.
    *   Riscos aceitos.
30. **`.gitignore`** (Modificar, fase 1). O `.gitignore` do fork (`/target`) mesclado com o local: `/concept_input.md`, `/concepts_brief.md`, `/design_draft.md`, `/review/`. O `/spike_results.md` sai do ignore, porque vai para `docs/designs/` (item 32).
31. **`.github/workflows/release.yml`** (Modificar, fase 6). O gatilho passa a ser `push` na `main` (não mais tag). O workflow compila os dois targets do Mac, gera `rrq-<target>.tar.gz` e `.sha256`, grava `version.txt` com `GITHUB_SHA` e substitui os assets da release rolante `main` (`gh release upload main … --clobber`; se a release não existir, ela é criada como prerelease). Sai a checagem contra `herdr-plugin.toml`. A atestação de proveniência (`gh attestation`) fica.
32. **`docs/designs/rex-review-queue.md`** e **`docs/designs/spike_results.md`** (Criar, fase 1). Este documento e as medições.

#### Fora do repo (passos manuais do autor, no README)

33. **macmini:** Rex.app como login item (auto-login conforme E2), build Rex do reviewr em `~/.local/bin/reviewr`, linha de base medida (E5).
34. **`~/Workspaces/dotfiles/rex/`:** nenhuma mudança na primeira entrega.

### Verification Plan

*   **Build**: `cargo build --release` sem warnings. Também `cargo clippy --all-targets -- -D warnings` e `cargo fmt --check`.
*   **Testes**: `cargo test`. Todos os testes existentes passam, adaptados aos fakes, mais os testes novos citados nos itens 5-27.
*   **Resíduos**: `rg -n -i herdr src/ Cargo.toml config.example.toml | rg -v 'OLD_AGENT_LABEL|tests/compat|herdr_state.json'` deve voltar vazio.
*   **Integração no macmini** (manual, roteiro no README, sessões descartáveis):
    1. `rrq doctor` passa.
    2. CUJ 1: um PR de teste fica `● pronto` e a aba `reviews` ganha o indicador no MacBook.
    3. CUJ 2: Enter troca a tela; o `tui.log` registra `enter … <500ms` no caso típico.
    4. CUJ 3: `c` abre Claude e reviewr; um segundo `c` não duplica; depois de `/exit` no Claude, `c` relança.
    5. CUJ 4 e 5: remoção com e sem Claude aberto; fechar a sessão à mão libera a remoção.
    6. CUJ 6: um setup que falha de propósito.
    7. CUJ 7: matar o TUI com `kill -ABRT` deixa um `error` no bloco e o indicador no MacBook; `rrq tui` de novo limpa.
    8. CUJ 9: um reboot real do macmini, seguido de `rrq tui` na sessão `reviews`.
    10. Update: push na `main`, depois `rrq update --check` mostra o SHA novo, e `rrq update` troca o binário e reinicia o daemon.
    9. CUJ 8: numa cópia do estado real antes da troca, e depois a troca de verdade.
*   **Uso real**: uma semana de reviews sem processo do Herdr, nenhuma worktree adotada ou suja perdida, e tempo até a worktree pronta dentro da linha de base (E5).

--------------------------------------------------------------------------------

<!-- Provenance: Indicates initial structural drafting via EGM. -->

*Drafted with EGM*
