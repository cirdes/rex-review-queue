# Estágio 0: resultados dos spikes

Rex CLI/servidor 0.1.0-preview.25 (API revision 24), app do Mac 0.1.0-preview.35. Data: 2026-11-26.

## Spike 3 (local): um processo num pane troca a sessão do app — OK

- `client.list` mostra o app com `info.kind = "app"` e `info.label = "macbook"`, além de clientes `cli` (cada chamada do `rex`).
- `client.inspect` lista as sessões às quais o app está **anexado** (`session_ids`, 3 no teste). Isso não informa qual sessão está **na tela**. `session.list_clients` de uma sessão também só diz quem está anexado.
- O app oferece a action `session.select` (args `session_id`, `window_id?`), visível em `rex -C macbook actions`.
- `client.perform {client_id, action: "session.select", args: {session_id}}` chamado de um processo dentro de um pane trocou a sessão do app e respondeu `{"performed_by": <client_id>}`. Não precisou ligar Remote Control.
- Latência, já contando a abertura de um processo `rex` por chamada: `session.create` levou cerca de 204 ms e `client.perform session.select` cerca de 115 ms. A meta de 500 ms do Enter até a troca cabe com folga, mesmo sem conexão persistente.
- Ponto em aberto: com mais de um app conectado (Mac e iPhone), nada indica qual deles está mostrando o painel. Proposta: escolher pelo `label` do cliente (configurável, padrão `macbook`) ou mandar para todos os apps `kind=app`. Ainda falta testar entre hosts (painel no macmini, app no MacBook).

## Spike 4 (local): fechar sessão — OK

- `session.destroy {session_id, undoable?}` fecha a sessão com todos os blocos. Com `undoable`, a sessão fica guardada por cerca de 1 min para `client.undo_close`.

## Spike 5 (parcial): OSC 7501 com registros-filho — o servidor guarda

- Um processo num pane emitiu a raiz (`idle`) e três filhos (`id=pr-101 done`, `pr-102 error`, `pr-103 working`, todos com `app=reviewq`, título e msg em base64).
- `rex block call -s <sessão> -b <bloco> program_status` devolveu os 4 registros com `state`, `title`, `msg` e `owner`.
- Pendente: como o app mostra isso (Activity, ícone da aba, notificação, badge). Precisa de alguém olhando a tela. A sessão `spike-status` ficou aberta para isso.
- Nota de uso da CLI: `rex block call` precisa de `-s <sessão>` quando o bloco é de outra sessão, e o método vai sem prefixo (`program_status`).

## Spike 1: servidor Rex headless no macmini — NÃO suportado oficialmente

- O macmini (`mini.cirdes`, arm64) responde por ssh e ainda não tem Rex instalado.
- `rex server install --help`: "Linux with systemd and Windows are supported so far. Elsewhere, `rex server start` starts a server that runs until the machine restarts." A instalação via `ssh://` também diz "Only Linux machines are supported so far."
- No macOS, o servidor fica a cargo do app (Settings > Server). Opções para o macmini:
  1. Instalar o Rex.app no macmini, com auto-login e o app como login item. É o caminho oficial, mas exige sessão gráfica.
  2. Criar um LaunchAgent próprio que roda `rex server run` (ou `start`). Não é oficial, pode conflitar com o app e precisa ser testado.
- Pendentes: como o MacBook alcança esse servidor (`rex hosts add macmini ssh://macmini` ou Tailscale) e se o LaunchAgent do daemon acha o socket.

## Instalação no macmini (feita em 2026-11-26, autorizada pelo autor)

- `/Applications/Rex.app` copiado para o macmini por rsync, junto com `~/Library/Application Support/com.superlogical.rex/RexServer` (182 MB). O CLI ficou em `~/.local/bin/rex`, como symlink para `.../RexServer/Versions/Current/Rex Server.app/Contents/MacOS/rex`. Versão igual à do MacBook (0.1.0-preview.25).
- `rex hosts add macmini ssh://macmini` no MacBook. A conexão por ssh sobe sozinha o servidor do macmini (`rex -S macmini whoami` mostra o socket em `~/Library/Application Support/rex/server.sock`). Até aqui não foi preciso abrir o app lá.
- Esse servidor subiu pelo ssh e não sobrevive a reboot. A persistência depende de abrir o Rex.app no macmini com auto-login e login item, que ainda não foi feito.
- Sessão de teste criada no macmini: `spike-remote`. Até aqui, o app do MacBook **não** aparece como cliente do servidor do macmini. Só os `cli` aparecem.

## Spike 3 entre hosts: processo no macmini troca a sessão do app do MacBook — OK

- O app do MacBook só passa a ser cliente do servidor do macmini depois que o autor conecta o host pela palette ("Connect Host…" / "Add Remote Host…"). Pedir isso ao app por `client.perform client.host.connect` devolveu `not_supported`. A partir daí, `rex -S macmini client ls` mostra `app macbook`.
- Um processo rodando no macmini (via ssh, usando o socket local de lá) criou a sessão `spike-remote-status` em cerca de 74 ms e trocou a tela do app do MacBook para ela com `client.perform session.select` em cerca de 101 ms (`performed_by` = o app). Os tempos já incluem a abertura de um processo `rex` por chamada.
- O painel no macmini acha o app pela lista de clientes do servidor local (`kind=app`). Não precisa de bridge nem de polling.
- Os registros 7501 da `spike-remote-status` ficam guardados no servidor do macmini (root idle, pr-101 done, pr-102 error, pr-103 working). Ainda falta saber como o app os mostra.

## Spike 5 remoto: status de sessão em segundo plano no macmini aparece no MacBook — OK

- A sessão `spike-bg` foi criada no macmini sem ir para a tela. Depois de 15 s, ela emitiu `pr-201 done` e `pr-202 blocked kind=question`. Na sidebar do MacBook, a aba `painel` da `spike-bg` ganhou um **ponto azul** (print do autor, 08:21:37). O ícone "zzz" é do processo `sleep` do script de teste.
- Com a sessão em foco, nenhum indicador aparece. É o comportamento documentado: o indicador vale para abas fora de foco.
- Consequência para o design: todos os registros por PR acabam num único indicador, na aba do painel. A sidebar não lista os PRs um a um. Não verificamos se a Activity (`cmd+.`) mostra o `msg` de cada registro-filho, nem se veio notificação do macOS ou som. Isso fica para o primeiro uso real, já que não muda a arquitetura.

## Spike 7: restart do servidor do macmini — IDs se mantêm, programa não volta

- `rex server stop` e depois `start` no macmini: os três `session_id` voltaram idênticos.
- Os blocos voltam com um shell novo (`login` → `zsh`). O programa que rodava (o script de teste) **não** é reaberto, e os registros 7501 somem (`records: []`).
- Consequência: depois de um restart ou reboot, alguém precisa reabrir o painel. Opções: o daemon garantir o painel a cada tick, como o reviewq já faz no Herdr (`daemon.rs:158-187`), e/ou o painel informar seu comando via OSC 7577 (Program Resume). Ao voltar, o painel emite de novo todos os registros a partir do estado do daemon.

## Spikes que ainda não rodaram

- 2: LaunchAgent do daemon falando com o servidor do macmini. O risco é baixo: o socket fica num caminho fixo do usuário (`~/Library/Application Support/rex/server.sock`) e o `rex` está em `~/.local/bin`. Validar no primeiro `service install`.
- 6: qual `init.lua` responde numa sessão remota. Só importa para a action da palette e a tecla global, que ficaram para o Estágio 2.
- Persistência do servidor do macmini depois de reboot: abrir o Rex.app lá como login item, com auto-login. Passo manual do autor.

## Veredito do Estágio 0

**Go.** As premissas de maior risco se confirmaram: troca de sessão a partir do painel (local e entre hosts) em cerca de 100 ms, criação e fechamento de sessão pela API, status 7501 de sessão remota em segundo plano chegando à sidebar do MacBook e IDs de sessão estáveis depois de restart. O que mudou no design: (1) o painel precisa ser reaberto depois de restart (o daemon garante o painel e/ou usa OSC 7577); (2) os avisos por PR aparecem como um único indicador na aba do painel; (3) o servidor do macmini depende do Rex.app como login item.

## Mini-spike de premissas do plano (2026-11-27, local, sessões `spike-rq2`/`spike-rq3`)

- **API revision:** `rex api call server.version` devolve `{"api_revision":24, "rex_version":"0.1.0-preview.25", ...}`.
- **Label de bloco na criação:** o layout do `session.create` (e do `session.new_split`) aceita `block.label`. Um bloco criado assim aparece com esse label no `session.view`. Também existem `session.set_block_label` e `session.focus_block`.
- **Bloco cujo comando termina:** por padrão o bloco fecha (com `exit.on_completion` em `true`), a não ser que o processo falhe em menos de 250 ms. Com `options.exit.on_completion=false` o bloco fica: o `process` devolve só `last_exit` (sem `foreground`), e o registro 7501 `error` emitido antes de sair continua lá. Um `write` nesse bloco devolve `bytes_written`, mas o texto não vai para nenhum programa.
- **Painel dentro de um shell (`options.initial_input = "<cmd>\r"`, sem `command`):** enquanto o programa roda, o foreground é ele. Quando o programa sai, o foreground volta a ser o shell (`name=zsh`, `argv0=-zsh`, cwd preservado), e o registro `error` **sobrevive** ao prompt do shell. Para reabrir, basta `block call write {"data":"<cmd>\r"}`: o programa volta a ser o foreground.
- **`block call process`:** o `foreground` e o `child` trazem `pid`, `name`, `path`, `invoked_path`, `argv0` e `cwd`, mas **não** trazem o argv completo. Um shell de login aparece com `argv0` igual a `-zsh`.
- **Erros:**
  - `client.perform` com `client_id` inexistente: `status=404 code=not_found`, exit 3.
  - `client.perform session.select` com sessão inexistente: `status=500 code=internal`, mensagem `…SessionNotFoundOnServerError…`, exit 1. Esse erro **não** sai como `not_found` limpo.
  - `session.view` com sessão inexistente: `status=404 code=session_not_found`, exit 3.
  - `block call` com bloco inexistente: `block target "…" not found`, exit 3.
- **`REX_BLOCK`:** definido no ambiente de todo bloco Rex. Esta sessão de trabalho roda com `REX_BLOCK=block:01a21767-…`.
- **`session.list`:** devolve só `session_id` e `label`, sem data de criação. Os IDs são UUIDv7 (ordenáveis por tempo).
