//! O que os cards mostram de um terminal: as últimas linhas úteis (sem a
//! moldura das TUIs) e, se a tela faz uma pergunta de sim/não, as teclas de
//! cada resposta. Puro: trabalha sobre as linhas da tela em texto.

use bytes::Bytes;

/// Quantas linhas úteis a prévia guarda.
const LINES: usize = 8;
/// Até quantas linhas acima do fim da tela procurar a opção destacada.
const MENU_SCAN: usize = 16;
/// Linhas com conteúdo permitidas abaixo das opções ("Esc to cancel"...).
const MENU_FOOTER: usize = 3;
/// Maior distância entre os traços da caixa de entrada (entrada multilinha).
const INPUT_BOX_MAX: usize = 12;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preview {
    /// Linhas úteis, da mais antiga para a mais recente.
    pub lines: Vec<String>,
    /// A tela pergunta sim/não.
    pub choice: Option<Choice>,
}

/// Bytes que respondem "sim" e "não" à pergunta na tela.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub yes: Bytes,
    pub no: Bytes,
}

/// Analisa as linhas da tela (do topo para baixo, sem espaços à direita).
/// `app_cursor`: o terminal está no modo de cursor de aplicação (setas `ESC O`).
pub fn analyze(rows: &[String], app_cursor: bool) -> Preview {
    let (end, choice) = match menu(rows, app_cursor) {
        Some((first, choice)) => (first, Some(choice)),
        None => match inline_question(rows) {
            Some(choice) => (rows.len(), Some(choice)),
            None => (input_box(rows).unwrap_or(rows.len()), None),
        },
    };
    let mut lines: Vec<String> = rows[..end]
        .iter()
        .rev()
        .filter(|r| is_meaningful(r) && !is_spinner(r))
        .map(|r| clean(r))
        .take(LINES)
        .collect();
    lines.reverse();
    Preview { lines, choice }
}

fn is_frame(c: char) -> bool {
    matches!(c, '\u{2500}'..='\u{259f}')
}

/// Linha com algo além de espaços e caracteres de moldura.
fn is_meaningful(text: &str) -> bool {
    text.chars()
        .any(|c| !c.is_whitespace() && !is_frame(c) && !matches!(c, '|' | '-' | '_' | '='))
}

/// Sem espaços e moldura nas pontas (`│ texto │`, o logo do Claude Code e o
/// `⎿` dos resultados de ferramenta, que as fontes do app nem têm).
fn clean(row: &str) -> String {
    row.trim_matches(|c: char| c.is_whitespace() || is_frame(c) || c == '⎿')
        .to_owned()
}

/// Spinner ou resumo de turno na coluna 0: "✻ Whisking… (3s)", "✻ Churned
/// for 11s". Muda a cada quadro e não diz o que o agente está fazendo.
fn is_spinner(row: &str) -> bool {
    let mut chars = row.chars();
    let glyph = chars.next().is_some_and(|c| {
        matches!(
            c,
            '·' | '✢' | '✳' | '✶' | '✻' | '✽' | '\u{2800}'..='\u{28ff}'
        )
    });
    glyph && chars.next() == Some(' ') && (row.contains('…') || row.contains(" for "))
}

/// Traço horizontal inteiro (`────`, `╭───╮`).
fn is_rule(row: &str) -> bool {
    let inner = row
        .trim()
        .trim_matches(|c| matches!(c, '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘' | '├' | '┤'));
    inner.chars().count() >= 8 && inner.chars().all(|c| matches!(c, '─' | '━'))
}

/// Caixa de entrada das TUIs: traço, `❯ texto`, traço. Devolve a linha do
/// traço de cima; dali para baixo é moldura (entrada, rodapé, statusline).
fn input_box(rows: &[String]) -> Option<usize> {
    let rules: Vec<usize> = (0..rows.len()).filter(|&i| is_rule(&rows[i])).collect();
    rules.windows(2).rev().find_map(|pair| {
        let (top, bottom) = (pair[0], pair[1]);
        let prompt = rows[top + 1].trim_start_matches(|c: char| c.is_whitespace() || c == '│');
        let is_box =
            (2..=INPUT_BOX_MAX).contains(&(bottom - top)) && prompt.starts_with(['❯', '>', '›']);
        is_box.then_some(top)
    })
}

/// Marcador da opção destacada num menu.
fn is_marker(c: char) -> bool {
    matches!(c, '❯' | '›' | '>' | '▶' | '►' | '→')
}

/// Uma linha como possível opção: coluna do texto, se está destacada e o texto.
/// `"  ❯ 1. Yes"` → `(4, true, "1. Yes")`.
fn option_parts(row: &str) -> (usize, bool, &str) {
    let indent = row.len() - row.trim_start_matches(' ').len();
    let rest = &row[indent..];
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(m), Some(' ')) if is_marker(m) => {
            let after = &rest[m.len_utf8() + 1..];
            let text = after.trim_start_matches(' ');
            (indent + 2 + after.len() - text.len(), true, text)
        }
        _ => (indent, false, rest),
    }
}

/// `"2. Yes, and…"` → `(Some(2), "Yes, and…")`.
fn numbered(text: &str) -> (Option<u32>, &str) {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    match text[digits..].strip_prefix(". ") {
        Some(label) if digits > 0 => (text[..digits].parse().ok(), label.trim_start()),
        _ => (None, text),
    }
}

/// O texto começa pela palavra (sem diferenciar maiúsculas): "Yes, proceed".
fn starts_with_word(label: &str, words: &[&str]) -> bool {
    let label = label.to_lowercase();
    words.iter().any(|w| {
        label
            .strip_prefix(w)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([',', ' ', '.', '!', '(']))
    })
}

fn is_yes(label: &str) -> bool {
    starts_with_word(label, &["yes", "sim"])
}

fn is_no(label: &str) -> bool {
    starts_with_word(label, &["no", "não", "nao"])
}

/// Atalho entre parênteses no fim da opção: "Yes, proceed (y)", "No (esc)".
fn shortcut(label: &str) -> Option<Bytes> {
    let inner = label.strip_suffix(')')?.rsplit_once('(')?.1;
    if inner.eq_ignore_ascii_case("esc") {
        return Some(Bytes::from_static(b"\x1b"));
    }
    let mut chars = inner.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(Bytes::from(c.to_string())),
        _ => None,
    }
}

struct MenuOption<'a> {
    number: Option<u32>,
    label: &'a str,
}

/// Menu de opções com sim e não no pé da tela (pedido de permissão, a
/// confiança na pasta do Claude Code, aprovação do Codex). Devolve a linha da
/// primeira opção e as teclas: o atalho da opção, o número dela ou setas
/// até ela a partir da destacada + Enter.
fn menu(rows: &[String], app_cursor: bool) -> Option<(usize, Choice)> {
    let last = rows.iter().rposition(|r| is_meaningful(r))?;
    let selected = (last.saturating_sub(MENU_SCAN)..=last)
        .rev()
        .find(|&i| option_parts(&rows[i]).1)?;
    let col = option_parts(&rows[selected]).0;
    // Opções começam na mesma coluna do texto da destacada; linhas mais
    // recuadas continuam a opção de cima (texto quebrado).
    let belongs = |row: &str| {
        let (c, marked, text) = option_parts(row);
        !text.is_empty() && !marked && c >= col
    };
    let mut first = selected;
    while first > 0 && belongs(&rows[first - 1]) {
        first -= 1;
    }
    while first < selected && option_parts(&rows[first]).0 != col {
        first += 1;
    }
    let mut end = selected + 1;
    while end < rows.len() && belongs(&rows[end]) {
        end += 1;
    }
    let footer = rows[end..].iter().filter(|r| is_meaningful(r)).count();
    if footer > MENU_FOOTER {
        return None;
    }

    let mut options = Vec::new();
    let mut current = 0;
    for (i, row) in rows.iter().enumerate().take(end).skip(first) {
        let (c, _, text) = option_parts(row);
        if c != col {
            continue;
        }
        if i == selected {
            current = options.len();
        }
        let (number, label) = numbered(text);
        options.push(MenuOption { number, label });
    }
    let numbers: Vec<Option<u32>> = options.iter().map(|o| o.number).collect();
    let in_order = (1..).zip(&numbers).all(|(n, got)| *got == Some(n));
    if options.len() < 2 || !(in_order || numbers.iter().all(Option::is_none)) {
        return None;
    }
    let yes = options.iter().position(|o| is_yes(o.label))?;
    let no = options.iter().position(|o| is_no(o.label))?;
    let keys = |target: usize| -> Bytes {
        let option = &options[target];
        if let Some(key) = shortcut(option.label) {
            return key;
        }
        if let Some(n @ 1..=9) = option.number {
            return Bytes::from(n.to_string());
        }
        let arrow: &[u8] = match (target > current, app_cursor) {
            (true, true) => b"\x1bOB",
            (true, false) => b"\x1b[B",
            (false, true) => b"\x1bOA",
            (false, false) => b"\x1b[A",
        };
        let mut out = arrow.repeat(target.abs_diff(current));
        out.push(b'\r');
        Bytes::from(out)
    };
    Some((
        first,
        Choice {
            yes: keys(yes),
            no: keys(no),
        },
    ))
}

/// Pergunta numa linha só, no fim da tela: "Continuar? (s/n)", "[Y/n]".
/// Responde com a inicial de cada opção + Enter.
fn inline_question(rows: &[String]) -> Option<Choice> {
    let row = rows.iter().rev().find(|r| is_meaningful(r))?.trim_end();
    let open = match row.chars().last()? {
        ')' => '(',
        ']' => '[',
        _ => return None,
    };
    let inner = &row[row.rfind(open)? + 1..row.len() - 1];
    let (a, b) = inner.split_once('/')?;
    let short_word =
        |w: &str| (1..=4).contains(&w.chars().count()) && w.chars().all(char::is_alphabetic);
    if !short_word(a) || !short_word(b) {
        return None;
    }
    let yes = a.chars().next()?.to_lowercase().next()?;
    let no = b.chars().next()?.to_lowercase().next()?;
    (matches!(yes, 'y' | 's') && no == 'n').then(|| Choice {
        yes: Bytes::from(format!("{yes}\r")),
        no: Bytes::from(format!("{no}\r")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(text: &str) -> Vec<String> {
        text.lines().map(|l| l.trim_end().to_owned()).collect()
    }

    fn of(text: &str) -> Preview {
        analyze(&screen(text), false)
    }

    fn choice(text: &str) -> (Bytes, Bytes) {
        let c = of(text).choice.expect("pergunta de sim/não");
        (c.yes, c.no)
    }

    #[test]
    fn claude_idle_skips_input_box_statusline_and_turn_summary() {
        let p = of(include_str!("fixtures/claude-idle.txt"));
        assert_eq!(p.choice, None);
        assert_eq!(
            p.lines.last().unwrap(),
            "verifiquei o conteúdo do arquivo, porque você pediu só o comando."
        );
        assert!(p.lines.iter().any(|l| l.starts_with("● Rodei sleep 6")));
        // Logo do Claude Code sem os blocos.
        assert_eq!(p.lines[0], "Claude Code v2.1.296");
        let all = p.lines.join("\n");
        for chrome in ["Haiku 5.5]", "manual mode", "Churned", "mostra o conteúdo"] {
            assert!(!all.contains(chrome), "{chrome} vazou na prévia:\n{all}");
        }
    }

    #[test]
    fn claude_running_shows_the_tool_not_the_spinner() {
        let p = of(include_str!("fixtures/claude-running.txt"));
        assert_eq!(p.choice, None);
        assert_eq!(
            &p.lines[p.lines.len() - 2..],
            ["Bash(sleep 6 && date > lento.txt)", "Running…"]
        );
    }

    #[test]
    fn claude_permission_prompt_answers_by_number() {
        let text = include_str!("fixtures/claude-permission.txt");
        assert_eq!(choice(text), (Bytes::from("1"), Bytes::from("4")));
        // A prévia termina na pergunta e mostra o comando acima dela.
        let p = of(text);
        assert_eq!(
            &p.lines[p.lines.len() - 2..],
            ["sleep 6 && date > lento.txt", "Do you want to proceed?"]
        );
    }

    #[test]
    fn claude_trust_dialog_navigates_from_the_highlighted_option() {
        let rows = screen(include_str!("fixtures/claude-trust.txt"));
        let c = analyze(&rows, false).choice.unwrap();
        // Destacado: "No, exit". Sim desce uma; não confirma a destacada.
        assert_eq!(c.yes, Bytes::from_static(b"\x1b[B\r"));
        assert_eq!(c.no, Bytes::from_static(b"\r"));
        let c = analyze(&rows, true).choice.unwrap();
        assert_eq!(c.yes, Bytes::from_static(b"\x1bOB\r"));
    }

    #[test]
    fn menu_shortcuts_win_over_numbers() {
        let text = "\
Would you like to run the following command?

  $ npm test

› 1. Yes, proceed (y)
  2. Yes, and don't ask again for this command (a)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel";
        assert_eq!(
            choice(text),
            (Bytes::from("y"), Bytes::from_static(b"\x1b"))
        );
    }

    #[test]
    fn menus_without_yes_and_no_or_far_from_the_bottom_are_ignored() {
        // Pergunta de múltipla escolha: não é sim/não.
        assert_eq!(
            of("Qual banco?\n❯ 1. Postgres\n  2. SQLite\n\nEnter to select").choice,
            None
        );
        // Menu já respondido, com saída depois dele.
        let old = "❯ 1. Yes\n  2. No\n● Bash(ls)\n  ⎿  a b\n● Pronto.\n  fim";
        assert_eq!(of(old).choice, None);
        // Lista numerada sem destaque, no meio da conversa.
        assert_eq!(of("● Opções:\n  1. Yes\n  2. No").choice, None);
        // Numeração fora de ordem.
        assert_eq!(of("❯ 1. Yes\n  3. No").choice, None);
    }

    #[test]
    fn inline_questions() {
        assert_eq!(
            choice("● Bash(npm install)\nPermitir npm install? (s/n)"),
            (Bytes::from("s\r"), Bytes::from("n\r"))
        );
        assert_eq!(
            choice("Overwrite? [Y/n]"),
            (Bytes::from("y\r"), Bytes::from("n\r"))
        );
        assert_eq!(
            choice("Apagar? (sim/não)"),
            (Bytes::from("s\r"), Bytes::from("n\r"))
        );
        // Respondida (o eco vem depois), ou parênteses que não são escolha.
        assert_eq!(of("Permitir? (s/n) s").choice, None);
        assert_eq!(of("Usage 2% (resets in 49m)").choice, None);
        assert_eq!(of("caminho (src/lib)").choice, None);
        // Um prompt de shell ocioso não é pergunta.
        assert_eq!(of("user@host:~/loja$").choice, None);
    }

    #[test]
    fn plain_output_keeps_the_last_lines() {
        let p = of("primeira\nsegunda\n╭──╮\n\n");
        assert_eq!(p.lines, ["primeira", "segunda"]);
        let many: String = (0..20).map(|i| format!("linha {i}\n")).collect();
        let p = of(&many);
        assert_eq!(p.lines.len(), LINES);
        assert_eq!(p.lines.last().unwrap(), "linha 19");
    }
}
