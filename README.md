# markdown-xterm
патч для xterm для корректной обработки языка разметки markdown

<img width="557" height="405" alt="изображение" src="https://github.com/user-attachments/assets/7c722d6c-393e-49a6-95c2-c6e88653be42" />

ниже описание от клода

# mdterm-bridge

**Markdown прямо в терминале.** Патч для `xterm` 411, который на лету превращает
markdown в цветной/жирный/курсивный текст. Вывод нейронок, `cat README.md`, заметки —
всё выглядит красиво без внешних программ и без пайпов.

## Что умеет

| Markdown | Как выглядит |
|---|---|
| `# ` … `###### ` | заголовки шести цветов (жёлтый, розовый, зелёный, голубой, красный, светло-серый), жирным, без `#` |
| `**жирный**`, `__жирный__` | жирный |
| `*курсив*`, `_курсив_` | курсив (`snake_case_names` и `my_file.txt` не трогает) |
| `` `код` `` | ярко-голубой |
| ```` ``` ```` блоки кода | ярко-голубой, обычной толщины; сами ```` ``` ```` скрыты; внутри ничего не форматируется |
| `- `, `* `, `+ ` | зелёная точка `•`, вложенность сохраняется |
| `> цитата` | приглушённый курсив с полоской слева |
| `---` | приглушённая линия |

Полноэкранные программы (`vim`, `less`, `htop`) и строки, где уже есть свои
цвета (`ls --color`), конвертер не трогает.

## Быстрая сборка

Нужны: `cargo` (Rust), `gcc`, `make`, `curl`, `patch` и dev-пакеты X11:

```sh
# Void
sudo xbps-install -S base-devel rust cargo curl patch libXaw-devel libXt-devel libXft-devel libXmu-devel libXpm-devel libXext-devel libX11-devel ncurses-devel
# Debian / Ubuntu
sudo apt install build-essential cargo curl patch libxaw7-dev libxt-dev libxft-dev libxmu-dev libxpm-dev libxext-dev libx11-dev libncurses-dev
# Fedora
sudo dnf install gcc make cargo curl patch libXaw-devel libXt-devel libXft-devel libXmu-devel libXpm-devel libXext-devel libX11-devel ncurses-devel
# Arch
sudo pacman -S --needed base-devel rust curl patch libxaw libxt libxft libxmu libxpm libxext libx11 ncurses
```

Дальше одна команда:

```sh
git clone https://github.com/<твой-ник>/mdterm-bridge.git
cd mdterm-bridge
./build.sh
```

Скрипт соберёт Rust-библиотеку, скачает исходники xterm 411, наложит патч и соберёт
xterm. Готовый бинарник: `build/xterm-411/xterm`.

```sh
build/xterm-411/xterm                                  # обычный запуск
build/xterm-411/xterm -e sh examples/demo.sh           # показать все возможности
MDTERM=0 build/xterm-411/xterm                         # конвертер выключен
```

> Не клади проект в путь с пробелами: `configure` не любит их в `LIBS`.

## Ручная сборка (если хочешь по шагам)

```sh
# 1. Rust-библиотека -> target/release/libmdterm_bridge.a
cargo build --release

# 2. исходники xterm 411 (нужна именно эта версия)
curl -L https://github.com/ThomasDickey/xterm-snapshots/archive/refs/tags/xterm-411.tar.gz | tar xz
mv xterm-snapshots-xterm-411 xterm-411
cd xterm-411

# 3. заголовок + патч
cp ../mdterm_bridge.h .
patch -p1 < ../xterm-411-mdterm.patch

# 4. сборка с линковкой библиотеки
LIBS="-L$(realpath ../target/release) -lmdterm_bridge -lpthread -ldl -lm" ./configure
make -j4
./xterm
```

Если у тебя уже есть распакованный `xterm-411` (например из tarball с
invisible-island.net), достаточно шагов 3–4. Патч накладывается **один раз**;
если `patch` пишет `Reversed (or previously applied) patch detected`, значит он
уже наложен: отвечай `n` (или используй `patch -N -p1 < ...`, он пропускает молча).

## Изменил `src/lib.rs`, как пересобрать

`make` не замечает, что поменялась `.a`-библиотека, поэтому бинарник нужно
удалить, иначе получишь старую версию:

```sh
cargo build --release
cd xterm-411        # или build/xterm-411
rm -f xterm
make
```

Или просто запусти `./build.sh` ещё раз.

### Поменять цвета

Цвета заголовков: функция `heading_style` в `src/lib.rs`, цвета кода:
константы `CODE_INLINE` / `CODE_BLOCK`. Это обычные ANSI SGR-коды
(`\x1b[1;35m` = жирный розовый, `\x1b[96m` = яркий голубой и т.д.).

## Как это устроено

xterm читает вывод программы из pty в `readPtyData()` (`ptydata.c`). Патч вставляет
туда фильтр: сырые байты идут в Rust (`mdterm_feed`), который держит недописанную
строку до `\n`, конвертирует markdown построчно в ANSI-последовательности и отдаёт
обратно. Дальше xterm видит обычный цветной текст.

- Строка показывается целиком по `\n`, чтобы `**жир` + `ный**` не разорвались.
- Если `\n` не пришёл 40 мс (приглашение шелла, набор символов), недописанная
  строка показывается как есть (константа `MDTERM_FLUSH_USEC` в `ptydata.c`).
- Если результат конвертации не влез в буфер xterm, остаток ставится в очередь и
  досылается позже (`charproc.c`: `mdterm_hook_flush_due`, `mdterm_hook_wait_usec`).
- В alt-screen (vim, less…) конвертер отключается.

## Ограничения

- Только построчный разбор, без полноценного CommonMark.
- Не поддерживаются: нумерованные списки (`1.`), таблицы, ссылки, картинки.
- Незакрытый блок ```` ``` ```` красит весь дальнейший вывод в голубой до следующего ```` ``` ````.
- Первые ~40 мс недописанной строки не конвертируются (см. выше).
- Патч сделан только под xterm **411**. На другие версии может лечь с ошибками:
  тогда смотри три вставки в `ptydata.c` / `charproc.c` и перенеси вручную.

## Тесты

```sh
cargo test      # тесты конвертера, xterm не нужен
```

## Лицензия

Код проекта (`src/lib.rs`, `mdterm_bridge.h`, `build.sh`) под лицензией на твой выбор
(добавь файл `LICENSE`, например MIT). Патч изменяет xterm, у которого своя
лицензия (X11/MIT-стиль), см. файл `COPYING` в исходниках xterm.
