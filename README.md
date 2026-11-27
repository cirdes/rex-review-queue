# rrq (rex-review-queue)

O `rrq` transforma a sua fila de reviews do GitHub em worktrees prontas para usar no [Rex](https://www.superlogical.com/rex).

Quando alguém pede a sua review num pull request, o daemon cria uma git worktree para o PR e roda os comandos de setup do repo (`mise install`, `bundle install`, `pnpm install` e assim por diante). Um painel no terminal mostra a fila e avisa no Rex quando a worktree fica pronta. Com Enter, o app do Rex no MacBook troca para uma sessão com um shell na worktree. Com `c`, a sessão ganha o Claude e o reviewr ao lado. Quando o PR sai da fila (você revisou, ou ele foi fechado ou mergeado), o daemon espera uma carência, fecha a sessão se ninguém estiver usando e remove a worktree, mas só se você nunca mexeu nela.

O `rrq` é o sucessor do herdr-reviewq, que fazia o mesmo sobre o Herdr.

## Onde cada parte roda

```text
 macmini                                             MacBook
┌────────────────────────────────────────────┐      ┌──────────────────────┐
│ LaunchAgent com.cirdes.rrq (rrq daemon)    │      │ Rex.app, conectado   │
│   GitHub (gh) → worktrees → setup          │      │ ao host macmini      │
│   state.json, sessions.json                │      │                      │
│                                            │      │                      │
│ Rex.app (servidor Rex, login item)         │      │                      │
│   sessão "reviews": bloco com `rrq tui` ───┼─────►│ indicador na aba     │
│   sessões "#123 repo", uma por PR aberto ◄─┼──────┤ Enter troca a tela   │
└────────────────────────────────────────────┘      └──────────────────────┘
```

O daemon e o painel rodam no macmini e falam com o servidor Rex de lá pelo CLI `rex`. O MacBook só mostra: o app do Rex se conecta ao host `macmini`, e o painel pede a ele que troque de sessão (`client.perform session.select`).

## Como funciona

- **O que entra na fila.** PRs em que a sua review foi pedida diretamente. Ficam de fora os drafts (entram quando saem de draft), os PRs de forks e os pedidos feitos só a um time do qual você faz parte.
- **Consulta.** O daemon consulta o GitHub pelo `gh` a cada `poll_interval_secs` (60 s por padrão).
- **Pedido novo.** Cria uma worktree na branch do PR e roda os passos de setup. Cada passo tem timeout próprio, e a saída vai para um log por PR, com segredos mascarados. A sessão do Rex não é criada aqui: ela nasce no primeiro Enter ou `c` no painel.
- **Commits novos.** A worktree é atualizada com `git reset --keep`, que se recusa a sobrescrever mudanças locais.
- **PR sai da fila.** O daemon espera `remove_grace_secs` (15 min por padrão), fecha a sessão do PR se ela estiver ociosa (ver [Sessão em uso](#sessão-em-uso)) e remove a worktree e a branch local que ele criou. Se a review for pedida de novo, a worktree volta.
- **Início do trabalho.** Quando o Claude ou o Codex aparece em foreground na sessão de um PR, com o diretório dentro da worktree, o daemon grava a hora e o agente. A conferência roda a cada `poll_interval_secs`, então a hora é a da primeira conferência que viu o agente. É só informativo e não protege a worktree da remoção.
- **CI, pilhas e os seus PRs.** A cada consulta, o daemon também lê o CI de cada PR da fila, os PRs abertos de cada repo (para achar PRs empilhados), os seus PRs abertos e quantos você abriu nas últimas 24 horas. Isso roda depois da fila, com um orçamento de 30 segundos, e nunca mexe em worktrees: se o GitHub ou o Rex falharem aqui, a fila não é afetada.
- **O seu trabalho fica a salvo.** Se você commitar, trocar de branch ou editar arquivos numa worktree, o `rrq` deixa a worktree onde está e mostra um alerta. Uma worktree que você trocou de branch fica *adotada*. Você também pode adotar uma worktree antes de começar a mexer nela. Worktree adotada nunca é removida sozinha.

## Requisitos

No macmini:

- macOS (o daemon roda como LaunchAgent do launchd).
- Rex.app instalado e aberto como login item, porque é ele que mantém o servidor Rex no ar. Sem auto-login, o servidor só volta depois de um reboot quando alguém faz login na máquina (ver [Passos manuais no macmini](#passos-manuais-no-macmini)).
- O CLI do Rex em `~/.local/bin/rex`, com API revision 24 ou mais nova. O daemon recusa subir com uma revision mais antiga, e o `rrq doctor` aponta a falha.
- O [GitHub CLI](https://cli.github.com) autenticado: `gh auth login` e depois `gh auth setup-git`.
- Um clone local de cada repo que você revisa.
- O que os comandos de setup precisarem, por exemplo o [mise](https://mise.jdx.dev).
- Para o `c`: o `claude` e a build do reviewr para Rex em `~/.local/bin/reviewr` (ou em outro diretório do PATH do LaunchAgent).
- Rust e cargo, só para compilar do código.

No MacBook:

- Rex.app conectado ao host `macmini` (palette: "Connect Host…"). O app precisa ter o label `macbook`, ou o label que você puser em `[rex] app_label`.

## Instalação

1. **Binário.** Baixe a build mais nova da `main` (troque o target por `x86_64-apple-darwin` num Mac Intel):

   ```sh
   target=aarch64-apple-darwin
   cd "$(mktemp -d)"
   gh release download main-build --repo cirdes/rex-review-queue --pattern "rrq-$target.tar.gz*"
   shasum -a 256 -c "rrq-$target.tar.gz.sha256"
   tar -xzf "rrq-$target.tar.gz"
   mkdir -p ~/.local/bin && mv rrq ~/.local/bin/rrq
   ```

   Para usar uma build sua, compile no clone (`cargo build --release`) e rode `./target/release/rrq start` no passo 3: o `start` copia o binário para `~/.local/bin/rrq`.

2. **Config.** Copie o exemplo para `~/.config/rrq/config.toml`:

   ```sh
   mkdir -p ~/.config/rrq
   gh api repos/cirdes/rex-review-queue/contents/config.example.toml \
     -H "Accept: application/vnd.github.raw" > ~/.config/rrq/config.toml
   ```

   - Em `[[repos]]`, ponha o `name` de cada repo (`owner/repo`), o `path` do clone local e os comandos de `setup`.
   - Em `disposable_ignored`, liste os caminhos ignorados pelo git que o setup cria (como `node_modules`, `vendor/bundle` ou `.husky/_` quando o install roda o husky). São os únicos arquivos ignorados que o `rrq` pode apagar junto com uma worktree.
   - `[rex]` e `[combo]` já vêm com padrões que servem na maioria dos casos. `[rex] bin` é o caminho absoluto do CLI `rex` (o LaunchAgent não herda o PATH do shell). `[combo]` define o que o `c` abre; `{worktree}` vira o caminho da worktree. Não ponha segredos no `[combo]`: os comandos aparecem no argv e no bloco.
   - Chave desconhecida na config é erro.

3. **Daemon.** Registre e inicie o LaunchAgent, e confira o resultado:

   ```sh
   ~/.local/bin/rrq start
   ~/.local/bin/rrq doctor
   ```

   O `doctor` imprime uma linha por checagem. Cada falha vem com a correção sugerida, e o comando sai com código 1 se alguma falhar.

4. **Painel.** No MacBook, crie uma vez uma sessão `reviews` no host `macmini` e rode nela:

   ```sh
   rrq tui
   ```

   O daemon não abre nem reabre o painel. Depois de um reboot do macmini, ou se o painel cair, rode `rrq tui` de novo nessa sessão.

## Atualização

Cada push na `main` publica uma build nova na release `main-build` do GitHub. Nada é instalado sem você pedir:

```sh
rrq update --check   # só diz se há build nova
rrq update           # baixa, confere o checksum, troca o binário e reinicia o daemon
```

Uma vez por dia o daemon compara o SHA da build instalada com o da `main-build`, e o rodapé do painel avisa quando há build nova. Um painel aberto continua no binário antigo até você sair dele e rodar `rrq tui` de novo, e o rodapé também avisa isso. `rrq --version` mostra a versão e o SHA do commit.

### Release

O workflow `.github/workflows/release.yml` roda a cada push na `main`. Ele compila os dois targets do Mac, gera `rrq-<target>.tar.gz` com o `.sha256` ao lado, grava o `version.txt` com o SHA do commit e troca os assets da prerelease `main-build`, criando a release se ela não existir. Os tarballs sobem antes do `version.txt`, então quem vê o SHA novo já acha os binários. Cada tarball tem atestação de proveniência: `gh attestation verify <arquivo> --repo cirdes/rex-review-queue`.

A release se chama `main-build`, e não `main`, porque a tag dela teria o mesmo nome do branch e o git passaria a achar `main` ambíguo.

## Comandos

| Comando | O que faz |
|---|---|
| `rrq start` | Copia o binário para `~/.local/bin/rrq`, grava o LaunchAgent `com.cirdes.rrq` e inicia o daemon. Recusa enquanto o LaunchAgent do herdr-reviewq estiver carregado. |
| `rrq stop` | Para o daemon e remove o LaunchAgent. |
| `rrq restart` | Reinicia o daemon, por exemplo depois de editar a config. |
| `rrq status` | Estado dos LaunchAgents, PRs da fila com CI, início e pilha, contagem do dia, PRs abertos nas últimas 24 horas, os seus PRs e os alertas. Um PR retido por sessão em uso aparece como `removing · session busy <tempo>`. |
| `rrq tui` | Abre o painel no bloco atual do Rex. |
| `rrq doctor` | Confere a config, o LaunchAgent antigo, o CLI e o servidor do Rex, a API revision, o app com o label configurado, os comandos do combo no PATH do LaunchAgent e as sessões de PR fora do `sessions.json`. Só lê: não cria sessão, não troca a tela do MacBook e não fecha nada. |
| `rrq update [--check]` | Ver [Atualização](#atualização). |

Comandos de apoio, que não aparecem na ajuda:

| Comando | O que faz |
|---|---|
| `rrq request sync` | Consulta o GitHub agora. |
| `rrq request retry --pr owner/repo#123` | Roda de novo um setup que falhou, ou recria uma worktree que sumiu (só worktrees gerenciadas). |
| `rrq request adopt --pr owner/repo#123` | Marca a worktree como sua antes de você mexer nela. |
| `rrq request release --pr owner/repo#123` | Devolve uma worktree adotada. Só funciona se ela estiver limpa e com tudo enviado. Depois que o PR é mergeado ou fechado, commits que nunca chegaram ao origin não impedem a devolução: ficam em `refs/reviewq/backup/pr-<n>/` por 14 dias, e o resultado avisa. A sessão do PR é fechada se estiver ociosa; se estiver em uso, ela fica aberta. |
| `rrq request release-all` | Devolve todas as worktrees adotadas. As que não estão limpas ou enviadas ficam, e o resultado lista quais. |
| `rrq daemon` | O daemon em si. Quem roda é o launchd. |

Os arquivos ficam em:

- `~/.config/rrq/config.toml`: a config.
- `~/.local/state/rrq/`: `state.json`, `sessions.json` (PR → sessão do Rex), `daemon.log`, `locks/` e, em `logs/`, um log de setup por PR e o `tui.log` do painel.

## Painel

```text
 reviewq   sync 0s
┌Pending─────┐┌Done today────┐┌Opened 24h────┐
│ 4          ││ –            ││ –            │
└────────────┘└──────────────┘└──────────────┘
  PR     State                         CI         Start  Author          Stacked Title
 Started
▸ #812 ● ready                                    15:16  bia             No      Corrige o cálculo do frete
 Pending
  #818 ✗ failed · pnpm install                    –      edu             No      Migra a tela de pedidos
  #817 ◐ preparing                                –      duda            No      Atualiza dependências
  #815 ● ready                                    –      caio            No      Adiciona filtro por categoria
 Leaving
  #801 ○ removing · session busy 25min            –      caio            No      Remove código morto

 / search · enter open · c combo · f next · o GitHub · l log · s sync · R retry · a adopt · r release · X rele
```

A lista tem estas seções, nesta ordem: **Started** (reviews pendentes em que um agente já foi aberto, as mais recentes primeiro), **Pending**, **My PRs** (os seus PRs abertos, os mais novos primeiro), **Adopted**, **No sync** e **Leaving**. Os contadores mostram as reviews pendentes, as reviews feitas hoje e os PRs que você abriu nas últimas 24 horas (`?` quando algum repo está faltando ou desatualizado).

### Colunas

| Coluna | O que mostra |
|---|---|
| `PR` | O número do PR (`#123`), ou `owner/repo#123` quando a config tem mais de um repo. |
| ícone | A worktree: `●` pronta, `◐` sendo criada ou preparada, `✗` setup falhou, `⊘` bloqueada, `○` sendo removida, `◆` adotada por você. Em **My PRs**, é o ícone do CI. |
| `State` | O estado da worktree por extenso (`ready`, `preparing`, `failed · <passo>`, `blocked · <motivo>`, `removing`), com detalhe onde importa: `leaves in Nmin` para um PR saindo da fila, `removing · session busy <tempo>` quando a sessão em uso segura a remoção, `yours · <motivo>` para uma worktree adotada, `GitHub not synced` quando a última consulta daquele repo falhou. Em **My PRs**: `open` ou `draft`. |
| `CI` | O CI do último commit do PR no GitHub: `◐` rodando, `✓` passou e há quanto tempo, `✗` falhou e o primeiro check que falhou, `?` desconhecido para o commit atual, vazio antes da primeira coleta. |
| `Start` | Quando você começou: a primeira vez que um agente (Claude ou Codex) foi visto na sessão do PR, como `HH:MM` hoje ou `Mon DD` (por exemplo `Nov 18`) em outros dias. `–` até isso acontecer. Não é a idade do PR. |
| `Author` | O autor do PR. `you` em **My PRs**. Some quando o painel é estreito. |
| `Stacked` | Se o PR está empilhado: `No`, ou `#<PR da base> i/n` com a posição na pilha. Um PR está empilhado quando a base dele é a branch de outro PR aberto no repo. Some antes do `Author` quando o painel é estreito. |
| `Title` | O título do PR, com a largura que sobrar. |

### Teclas

| Tecla | O que faz |
|---|---|
| `↑` `↓` / `k` `j` | Move a seleção. |
| `/` | Busca (ver [Busca](#busca)). |
| `enter` | Troca o app do MacBook para a sessão do PR. Na primeira vez, cria a sessão `#123 repo` (ou `owner/repo#123` com repos de mais de um dono) com um shell na worktree. Se a sessão foi fechada à mão, cria de novo. Funciona em qualquer fase com worktree, menos enquanto ela é criada ou removida. Num PR seu (**My PRs**), vai para a worktree onde a branch do PR está, ou cria uma em `worktrees_dir/<owner>/<repo>/mine-pr-<n>`. |
| `c` | Faz o mesmo que o Enter e abre o combo: o `claude` à direita do shell e o reviewr à direita do `claude`, com 55% da largura dos dois. Bloco que já existe com o programa rodando fica como está; bloco cujo programa saiu ganha o comando de novo. Repetir o `c` nunca duplica blocos, e um `c` que falhou no meio é completado pelo seguinte. O foco termina no `claude`. |
| `f` | Vai para a review pronta mais antiga, como o Enter. |
| `o` | Copia a URL do PR para o clipboard do MacBook (OSC 52). |
| `l` | Abre o log de setup do PR num bloco à direita do painel, com `less +F`. |
| `s` | Consulta o GitHub agora. |
| `R` | Tenta de novo (só worktrees gerenciadas, com setup falho ou worktree ausente). |
| `a` | Adota a worktree (pede `y`/`n`). |
| `r` | Devolve uma worktree adotada (pede `y`/`n`). |
| `X` | Devolve todas as worktrees adotadas (pede `y`/`n`). As que têm trabalho não commitado, ou não enviado num PR ainda aberto, ficam, e a linha de resultado lista quais. |
| `q` / `esc` / `ctrl+c` | Sai. Com uma busca ativa, `esc` limpa a busca. |

Em **My PRs**, só `enter` e `o` fazem algo. Uma worktree criada pelo Enter num PR seu é sua: o `rrq` não roda o setup nela, não acompanha e nunca remove. Se a branch já existe localmente, ela é usada como está; senão, começa em `origin/<branch>` com o upstream configurado.

O Enter leva cerca de 100 ms quando a sessão já existe e até uns 300 ms quando precisa criá-la. O `tui.log` registra a duração de cada Enter. Sem o app com o label configurado conectado, o Enter avisa e não troca a tela de nenhum outro app.

### Busca

`/` filtra a lista enquanto você digita. Só as linhas que batem ficam, ainda agrupadas nas seções, e o rodapé mostra quantas (`/eng-4321▏ (2 of 23)`).

- **Onde procura:** número do PR (`123` ou `#123`), repo, autor, título e branch. A branch costuma ter o ID da issue do Linear (`alice-eng-4321-fix`), então `eng-4321` acha o PR mesmo que o título não cite a issue.
- **Como compara:** aproximado, sem diferenciar maiúsculas nem acentos. Uma palavra bate com um campo quando as letras dela aparecem ali na ordem, com buracos permitidos: `crimp` acha "Correct importation". As letras precisam vir do mesmo campo. Várias palavras precisam bater todas: `alice tela`.
- **Ordem:** dentro de cada seção, os melhores resultados primeiro. Um trecho exato vale mais do que letras espalhadas.
- **Enquanto digita,** toda letra vai para a busca (`q`, `s`, `a` não disparam ações). `↑` `↓` movem a seleção, `backspace` apaga, `ctrl+u` limpa, `enter` mantém o filtro e devolve as teclas, `esc` limpa.
- **Com o filtro mantido,** a linha do topo mostra `search "<texto>": n of m`. O filtro sobrevive às atualizações da lista; `/` edita e `esc` limpa. Os contadores ignoram o filtro.

## Status nativo

O painel avisa o MacBook pelo protocolo de status do Rex (OSC 7501). Todos os avisos acabam num único indicador, na aba da sessão `reviews` na sidebar do MacBook. O indicador só aparece quando a aba está fora de foco.

- **Raiz (`reviewq`):** fica `error` enquanto houver uma condição ativa e volta a `idle` quando ela some. As condições são: `gh` sem autenticação, algum repo com a última consulta falha e `sessions.json` corrompido. Com mais de uma, a mensagem mostra a primeira nessa ordem.
- **Um registro por PR:**
  - criando ou preparando a worktree: `working`;
  - pronta e você ainda não entrou: `done`, "Review pronta: <título>";
  - setup falhou: `error`, com o passo;
  - bloqueada (o daemon parou de mexer na worktree por um motivo que exige você): `blocked`;
  - você entrou, o PR saiu da fila ou foi removido: o registro some.
- Ao abrir, o painel limpa tudo e emite de novo a partir do estado.
- O Rex decide quando apagar um `done`, por exemplo quando você aperta uma tecla na aba. O painel continua mostrando o PR como `● ready`.
- Só um painel emite. Um segundo `rrq tui` aberto ao mesmo tempo funciona, mas não emite status, e o rodapé dele avisa isso.
- Sair com `q`, `esc` ou `ctrl+c` limpa o indicador. Se o painel cair (erro ou pânico), ele deixa um `error` com o motivo. O bloco continua no shell, e `rrq tui` de novo limpa o aviso.

Com o painel fechado, os avisos não chegam ao MacBook. O estado continua sendo atualizado e tudo aparece quando o painel abre de novo.

## Sessão em uso

Quando a carência de um PR que saiu da fila acaba, o daemon fecha a sessão do PR antes de remover a worktree, mas só se ela estiver ociosa. Ociosa quer dizer que, em todos os blocos, o processo em foreground é um shell parado no prompt, com o diretório dentro da worktree.

- Se algum bloco tem outro programa em foreground (o Claude, um `vim`, um servidor), a sessão está em uso. O PR fica em `removing`, o painel e o `rrq status` mostram `session busy <tempo>`, e o daemon tenta de novo a cada consulta.
- Para liberar, feche o programa ou a sessão. A remoção acontece na consulta seguinte.
- Uma sessão fechada à mão, ou que sumiu, não impede nada: o daemon segue para a remoção.
- O daemon só fecha sessões que estão no `sessions.json`, ou seja, as que o painel criou. Sessões criadas à mão nunca são fechadas. Se o `sessions.json` se perder, o `rrq doctor` lista as sessões `#<n> <repo>` que ficaram de fora, para você fechar.
- Uma worktree que mudou enquanto a sessão era fechada fica no lugar, como adotada.
- A regra só olha o foreground. Um job em background num shell não segura a remoção e morre junto com a sessão.

## Migração do herdr-reviewq

Não há comando de importação. O `state.json` é compatível nos dois sentidos: o `rrq` lê o estado do herdr-reviewq e o herdr-reviewq lê o estado do `rrq`. No macmini:

1. Pare o daemon antigo:

   ```sh
   herdr-reviewq service uninstall
   ```

2. Copie o estado:

   ```sh
   mkdir -p ~/.local/state/rrq
   cp ~/.local/state/herdr-reviewq/state.json ~/.local/state/rrq/
   ```

3. Copie a config e tire as chaves do Herdr (`herdr_session`, `backend`, `window` e a seção `[notify]`). Acrescente `[rex]` e `[combo]` só se os padrões não servirem:

   ```sh
   mkdir -p ~/.config/rrq
   cp ~/.config/herdr-reviewq/config.toml ~/.config/rrq/
   ```

   Se sobrar alguma chave antiga, o `rrq` recusa a config e o `rrq doctor` aponta cada uma pelo nome.

4. Instale o binário (ver [Instalação](#instalação)) e inicie o daemon:

   ```sh
   rrq start
   ```

5. Confira:

   ```sh
   rrq doctor
   ```

Um PR cuja worktree não existe mais continua no estado e é tratado como worktree ausente (`R` recria). O `rrq start` e o daemon recusam rodar enquanto o LaunchAgent `dev.cirdes.herdr-reviewq` estiver carregado, e o `rrq doctor` aponta isso como falha. Assim os dois daemons nunca mexem nas mesmas worktrees.

### Rollback

É o caminho inverso, levando de volta o `state.json` **atual** do `rrq`, e não uma cópia de antes da troca. Assim as worktrees criadas ou adotadas depois do corte não se perdem.

```sh
rrq stop
cp ~/.local/state/rrq/state.json ~/.local/state/herdr-reviewq/
herdr-reviewq service install
```

A config antiga continua em `~/.config/herdr-reviewq/` se você copiou em vez de mover. As sessões do Rex abertas pelo `rrq` não são fechadas pelo herdr-reviewq: feche à mão as que não quiser mais.

## Linha de base do tempo até a worktree pronta

Antes da troca, meça a mediana dos últimos 30 dias no `daemon.log` do herdr-reviewq e anote aqui. Depois de uma semana de uso, rode o mesmo cálculo no log do `rrq` e compare. O cálculo pega, para cada PR, o tempo entre o início do setup e a worktree pronta:

```sh
cat > /tmp/rrq-baseline.py <<'EOF'
import re, sys, statistics
from datetime import datetime, timedelta
start, durs = {}, []
limit = datetime.now() - timedelta(days=30)
for line in open(sys.argv[1], errors="replace"):
    m = re.match(r"(\S+ \S+) (\S+#\d+): (setup iniciado|pronto|ready)$", line.rstrip("\n"))
    if not m:
        continue
    at = datetime.strptime(m[1], "%Y-%m-%d %H:%M:%S")
    if at < limit:
        continue
    if m[3] == "setup iniciado":
        start[m[2]] = at
    elif m[2] in start:
        durs.append((at - start.pop(m[2])).total_seconds())
if durs:
    print(f"{len(durs)} setups, mediana {statistics.median(durs) / 60:.1f} min")
else:
    print("nenhum setup nos últimos 30 dias")
EOF
python3 /tmp/rrq-baseline.py ~/.local/state/herdr-reviewq/daemon.log   # antes da troca
python3 /tmp/rrq-baseline.py ~/.local/state/rrq/daemon.log             # depois
```

| Medição | Setups | Mediana |
|---|---|---|
| herdr-reviewq, 30 dias antes da troca | a medir | a medir |
| `rrq`, primeira semana | a medir | a medir |

## Passos manuais no macmini

Estes passos ficam fora do repo e são feitos uma vez, à mão:

1. **Rex.app como login item.** Abra o Rex.app no macmini e adicione-o aos itens de login (Ajustes do Sistema > Geral > Itens de Início). É ele que mantém o servidor Rex no ar.
2. **Auto-login (decisão pendente).** Sem auto-login, o servidor Rex não volta depois de um reboot até alguém fazer login. Com auto-login, quem tiver acesso físico ao macmini tem uma sessão aberta com o `gh` e as chaves ssh, e o FileVault não pode desbloquear no boot.
3. **reviewr para Rex** em `~/.local/bin/reviewr`.
4. **MacBook conectado ao host.** No app do MacBook, conecte o host `macmini` pela palette ("Connect Host…").
5. **Linha de base.** Meça e anote a [linha de base](#linha-de-base-do-tempo-até-a-worktree-pronta) antes de migrar.
6. **Migração.** Siga [Migração do herdr-reviewq](#migração-do-herdr-reviewq).
7. **Roteiro de verificação**, com sessões descartáveis:
   - `rrq doctor` passa;
   - um PR de teste fica `● ready` e a aba `reviews` ganha o indicador no MacBook;
   - Enter troca a tela, e o `tui.log` registra `enter … <500ms` no caso típico;
   - `c` abre o Claude e o reviewr, um segundo `c` não duplica, e depois de `/exit` no Claude o `c` relança;
   - a remoção funciona com e sem o Claude aberto, e fechar a sessão à mão libera a remoção;
   - um setup que falha de propósito aparece como `error`;
   - `kill -ABRT` no painel deixa um `error` no bloco, e `rrq tui` de novo limpa;
   - depois de um reboot real do macmini, `rrq tui` na sessão `reviews` volta a funcionar;
   - depois de um push na `main`, `rrq update --check` mostra o SHA novo e `rrq update` troca o binário e reinicia o daemon.

## Garantias

- **Só worktrees intocadas são removidas, e só depois da carência.** Intocada quer dizer:
  - mesma branch e mesmo commit que o `rrq` fez checkout;
  - nenhuma mudança em arquivo rastreado e nenhum arquivo novo;
  - nenhum arquivo ignorado fora de `disposable_ignored`.
- **Nenhum comando git destrutivo.** O `rrq` nunca roda `reset --hard`, `clean` nem `--force`. As atualizações usam `reset --keep`.
- **Branches locais só são apagadas quando o `rrq` as criou**, ainda apontam para o commit dele e nenhuma outra worktree está nelas.
- **Backup antes de mudar.** Antes de qualquer reset ou remoção, o commit anterior vai para `refs/reviewq/backup/pr-<n>/<timestamp>` e fica 14 dias.
- **Falha não apaga nada.** Uma consulta ao GitHub que falha nunca remove worktree, e uma falha ao gravar o estado interrompe o ciclo.
- **Sessão em uso não é fechada**, e sessão que o `rrq` não criou nunca é fechada.
- **O diff bate com o do GitHub.** O `rrq` busca a branch base do PR (para um PR empilhado, a branch do PR de baixo) em `origin/<base>` ao criar a worktree, a cada push do autor e quando o GitHub muda a base do PR. Branches locais, como a `main` do clone, nunca são tocadas, então compare com `origin/<base>`, e não com `main`.
- **O reviewr abre na base certa.** Em cada worktree, a base fica gravada como a escolha de base do reviewr (ref `refs/worktree/reviewr/base-pick`). Uma base que você escolheu com `B` nunca é sobrescrita.

## Riscos aceitos

- **Painel fechado ou caído:** os avisos não chegam ao MacBook até você abrir o painel de novo.
- **Job em background num shell:** morre quando a sessão é fechada, porque a regra de sessão em uso só olha o foreground. O Herdr fazia o mesmo.
- **Sessão em uso por muito tempo:** a worktree não é removida enquanto isso. O painel mostra há quanto tempo.
- **Código de PR controla o app do MacBook:** o setup roda scripts dos PRs com o mesmo usuário do servidor Rex e, por isso, consegue falar com o app do MacBook. Os PRs são de colegas e os de forks são ignorados.
- **Rex em preview:** uma atualização do Rex pode mudar a API. O daemon e o `doctor` conferem a API revision mínima, e os testes usam respostas capturadas da revision 24.
