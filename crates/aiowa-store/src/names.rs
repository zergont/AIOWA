//! Имена людей в запросе на обычном языке (`docs/ai-layer-design.md` § 5.2): начальные формы
//! слова-имени («Иванова», «Иванову» → «Иванов»; «Васи», «Васе» → «Вася»), уменьшительные имена
//! («Вася» → «Василий») и поиск человека среди корреспондентов ящика.
//!
//! Словаря фамилий нет: начальная форма не вычисляется, а угадывается — слово порождает несколько
//! кандидатов, и настоящим считается тот, что встретился среди имён корреспондентов.

use crate::text::norm_lower;
use std::collections::HashSet;

/// Уменьшительное имя → полные имена.
const DIMINUTIVES: &[(&str, &[&str])] = &[
    ("саша", &["александр", "александра"]),
    ("саня", &["александр"]),
    ("санек", &["александр"]),
    ("шура", &["александр", "александра"]),
    ("шурик", &["александр"]),
    ("леша", &["алексей"]),
    ("алеша", &["алексей"]),
    ("леха", &["алексей"]),
    ("толя", &["анатолий"]),
    ("толик", &["анатолий"]),
    ("андрюша", &["андрей"]),
    ("антоша", &["антон"]),
    ("тоша", &["антон"]),
    ("аркаша", &["аркадий"]),
    ("боря", &["борис"]),
    ("вадик", &["вадим"]),
    ("валя", &["валентин", "валентина"]),
    ("валера", &["валерий"]),
    ("лера", &["валерия", "валерий"]),
    ("вася", &["василий"]),
    ("витя", &["виктор"]),
    ("вика", &["виктория"]),
    ("виталик", &["виталий"]),
    ("володя", &["владимир"]),
    ("вова", &["владимир"]),
    ("владик", &["владислав", "владимир"]),
    ("влад", &["владислав", "владимир"]),
    ("слава", &["вячеслав", "владислав", "станислав", "ярослав", "святослав"]),
    ("славик", &["вячеслав", "владислав", "станислав"]),
    ("гена", &["геннадий"]),
    ("жора", &["георгий"]),
    ("гоша", &["георгий", "игорь"]),
    ("гриша", &["григорий"]),
    ("даня", &["даниил", "данил", "данила"]),
    ("дима", &["дмитрий"]),
    ("митя", &["дмитрий"]),
    ("женя", &["евгений", "евгения"]),
    ("ваня", &["иван"]),
    ("илюша", &["илья"]),
    ("костя", &["константин"]),
    ("леня", &["леонид"]),
    ("макс", &["максим"]),
    ("миша", &["михаил"]),
    ("коля", &["николай"]),
    ("паша", &["павел"]),
    ("петя", &["петр"]),
    ("рома", &["роман"]),
    ("сережа", &["сергей"]),
    ("серега", &["сергей"]),
    ("стас", &["станислав"]),
    ("стасик", &["станислав"]),
    ("степа", &["степан"]),
    ("сеня", &["семен", "арсений"]),
    ("федя", &["федор"]),
    ("эдик", &["эдуард"]),
    ("юра", &["юрий"]),
    ("юрик", &["юрий"]),
    ("яша", &["яков"]),
    ("аня", &["анна"]),
    ("настя", &["анастасия"]),
    ("тоня", &["антонина"]),
    ("варя", &["варвара"]),
    ("галя", &["галина"]),
    ("даша", &["дарья"]),
    ("катя", &["екатерина"]),
    ("лена", &["елена"]),
    ("лиза", &["елизавета"]),
    ("зина", &["зинаида"]),
    ("ира", &["ирина"]),
    ("ксюша", &["ксения"]),
    ("лара", &["лариса"]),
    ("лида", &["лидия"]),
    ("лиля", &["лилия"]),
    ("люба", &["любовь"]),
    ("люда", &["людмила"]),
    ("люся", &["людмила"]),
    ("мила", &["людмила"]),
    ("рита", &["маргарита"]),
    ("маша", &["мария"]),
    ("надя", &["надежда"]),
    ("наташа", &["наталья", "наталия"]),
    ("оля", &["ольга"]),
    ("поля", &["полина"]),
    ("рая", &["раиса"]),
    ("света", &["светлана"]),
    ("соня", &["софья", "софия"]),
    ("тома", &["тамара"]),
    ("таня", &["татьяна"]),
    ("юля", &["юлия"]),
];

/// Полные имена, которых нет среди значений словаря уменьшительных.
const FIRST_NAMES: &[&str] = &[
    "артем", "артур", "денис", "егор", "игорь", "илья", "кирилл", "никита", "олег", "руслан", "тимур", "алина", "алла", "вера", "вероника", "диана", "жанна", "зоя", "инна", "карина", "кира", "кристина", "марина", "нина", "оксана", "яна",
    "наталья", "наталия",
];

/// Основы с беглой гласной: «Павла» → «Павел», «Льва» → «Лев».
const FLEETING: &[(&str, &str)] = &[("павл", "павел"), ("льв", "лев")];

/// Слово — личное имя (полное или уменьшительное), а не фамилия или должность.
pub fn is_first_name(word: &str) -> bool {
    DIMINUTIVES.iter().any(|(d, full)| *d == word || full.contains(&word)) || FIRST_NAMES.contains(&word)
}

/// Полные имена уменьшительного: «вася» → «василий».
fn full_names(word: &str) -> &'static [&'static str] {
    DIMINUTIVES.iter().find(|(d, _)| *d == word).map(|(_, full)| *full).unwrap_or(&[])
}

/// Возможные начальные формы слова-имени, само слово — первым. Слово — в нижнем регистре, ё → е.
/// «иванова» → иванова, иванов; «ивановой» → иванова; «васи» → вася; «василию» → василий;
/// «игорем» → игорь; «белого» → белый, белой.
pub fn nominatives(word: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![word.to_string()];
    if !word.chars().all(|c| ('а'..='я').contains(&c)) {
        return out;
    }
    // (окончание, чем его заменить)
    const RULES: &[(&str, &[&str])] = &[
        ("ого", &["ый", "ий", "ой"]),
        ("его", &["ий", "ый"]),
        ("ому", &["ый", "ий", "ой"]),
        ("ему", &["ий", "ый"]),
        ("ым", &["", "ый", "ой"]),
        ("им", &["", "ий"]),
        ("ой", &["а", "ая"]),
        ("ей", &["а", "я", "яя", "ь"]),
        ("ую", &["ая"]),
        ("юю", &["яя"]),
        ("ом", &["", "ый", "ой"]),
        ("ем", &["", "ь", "й", "ий"]),
        ("ии", &["ия", "ий"]),
        ("а", &[""]),
        ("я", &["ь", "й"]),
        ("у", &["", "а"]),
        ("ю", &["ь", "й", "я"]),
        ("е", &["", "а", "я", "ь", "й"]),
        ("и", &["а", "я", "ь", "й"]),
        ("ы", &["а"]),
    ];
    for (ending, replacements) in RULES {
        let Some(base) = word.strip_suffix(ending) else { continue };
        if base.chars().count() < 2 {
            continue;
        }
        for r in *replacements {
            out.push(format!("{base}{r}"));
            if r.is_empty()
                && let Some((_, full)) = FLEETING.iter().find(|(f, _)| *f == base)
            {
                out.push((*full).to_string());
            }
        }
    }
    let mut seen = HashSet::new();
    out.retain(|w| seen.insert(w.clone()));
    out
}

/// Слова имени, которыми может быть названо слово запроса: начальные формы и полные имена
/// уменьшительных («васи» → васи, вася, василий).
pub fn name_words(word: &str) -> Vec<String> {
    let mut out = nominatives(word);
    let full: Vec<String> = out.iter().flat_map(|w| full_names(w).iter().map(|f| (*f).to_string())).collect();
    for f in full {
        if !out.contains(&f) {
            out.push(f);
        }
    }
    out
}

/// Корреспондент ящика: адрес, имя и вес (частота с преимуществом свежих писем).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonEntry {
    pub name: Option<String>,
    pub email: String,
    pub weight: i64,
    /// Слова имени в нижнем регистре, ё → е.
    words: Vec<String>,
    /// Слова адреса до `@` («ivan.petrov» → ivan, petrov).
    local: Vec<String>,
}

/// Человек, найденный по словам запроса: имя и все его адреса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonMatch {
    /// Имя для чипа; адрес, если имени нет.
    pub label: String,
    pub emails: Vec<String>,
    pub weight: i64,
}

/// Все корреспонденты ящика по убыванию веса и слова их имён.
#[derive(Debug, Clone, Default)]
pub struct PeopleIndex {
    entries: Vec<PersonEntry>,
    vocab: HashSet<String>,
}

fn split_words(s: &str) -> Vec<String> {
    norm_lower(s).split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect()
}

impl PeopleIndex {
    /// `people` — (имя, адрес, вес) по убыванию веса.
    pub fn new(people: impl IntoIterator<Item = (Option<String>, String, i64)>) -> Self {
        let mut entries = Vec::new();
        let mut vocab = HashSet::new();
        for (name, email, weight) in people {
            let words = name.as_deref().map(split_words).unwrap_or_default();
            let local = split_words(email.split('@').next().unwrap_or(""));
            vocab.extend(words.iter().cloned());
            entries.push(PersonEntry { name, email, weight, words, local });
        }
        Self { entries, vocab }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Слово точь-в-точь есть среди слов имён корреспондентов.
    pub fn has_name_word(&self, word: &str) -> bool {
        self.vocab.contains(word)
    }

    /// Люди, названные словами `mention` (в нижнем регистре, ё → е): каждое слово должно совпасть
    /// со словом имени человека (в начальной форме, с уменьшительными) или со словом его адреса.
    /// Точных совпадений нет — по началу слова (от четырёх букв: «диспетчера» → «Диспетчерская»).
    /// Люди с одним именем и разными адресами — один человек. По убыванию веса.
    pub fn resolve(&self, mention: &[String]) -> Vec<PersonMatch> {
        if mention.is_empty() {
            return Vec::new();
        }
        let alts: Vec<Vec<String>> = mention.iter().map(|w| name_words(w)).collect();
        let exact = |e: &PersonEntry| alts.iter().all(|a| a.iter().any(|w| e.words.contains(w) || e.local.contains(w)));
        let prefix = |e: &PersonEntry| {
            alts.iter().all(|a| {
                a.iter().any(|w| e.words.contains(w) || e.local.contains(w))
                    || a.iter().filter(|w| w.chars().count() >= 4).any(|w| e.words.iter().chain(&e.local).any(|n| n.starts_with(w.as_str())))
            })
        };
        let mut found: Vec<&PersonEntry> = self.entries.iter().filter(|e| exact(e)).collect();
        if found.is_empty() {
            found = self.entries.iter().filter(|e| prefix(e)).collect();
        }
        // Один человек — одно имя; адреса без имени — каждый сам по себе.
        let mut out: Vec<PersonMatch> = Vec::new();
        for e in found {
            let key = e.name.as_deref().map(norm_lower).filter(|n| !n.trim().is_empty());
            let same = key.as_ref().and_then(|k| out.iter_mut().find(|m| norm_lower(&m.label) == *k));
            match same {
                Some(m) => {
                    m.emails.push(e.email.clone());
                    m.weight += e.weight;
                }
                None => out.push(PersonMatch { label: e.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| e.email.clone()), emails: vec![e.email.clone()], weight: e.weight }),
            }
        }
        out.sort_by_key(|m| std::cmp::Reverse(m.weight));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> PeopleIndex {
        PeopleIndex::new([
            (Some("Иванов Иван Петрович".to_string()), "ivanov@x.ru".to_string(), 50),
            (Some("Петров Василий Ильич".to_string()), "v.petrov@x.ru".to_string(), 40),
            (Some("Иванова Анна Сергеевна".to_string()), "a.ivanova@x.ru".to_string(), 30),
            (Some("Диспетчерская служба".to_string()), "ods@x.ru".to_string(), 25),
            (Some("Васильев Пётр Александрович".to_string()), "vasilev@y.ru".to_string(), 20),
            (Some("Петров Василий Ильич".to_string()), "petrov.old@y.ru".to_string(), 5),
            (Some("Белый Александр Игоревич".to_string()), "bely@x.ru".to_string(), 4),
            (None, "noreply@service.com".to_string(), 3),
            (Some("Смирнов Василий Олегович".to_string()), "smirnov@x.ru".to_string(), 2),
        ])
    }

    fn mention(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn labels(m: &[PersonMatch]) -> Vec<&str> {
        m.iter().map(|p| p.label.as_str()).collect()
    }

    #[test]
    fn nominative_candidates() {
        let has = |word: &str, expected: &str| assert!(nominatives(word).iter().any(|w| w == expected), "{word} → {expected}: {:?}", nominatives(word));
        has("иванова", "иванов");
        has("иванова", "иванова");
        has("иванову", "иванов");
        has("иванову", "иванова");
        has("ивановым", "иванов");
        has("иванове", "иванов");
        has("ивановой", "иванова");
        has("васи", "вася");
        has("васе", "вася");
        has("васю", "вася");
        has("васей", "вася");
        has("василия", "василий");
        has("василию", "василий");
        has("василием", "василий");
        has("василии", "василий");
        has("игоря", "игорь");
        has("игорем", "игорь");
        has("андрея", "андрей");
        has("анны", "анна");
        has("анне", "анна");
        has("марии", "мария");
        has("марию", "мария");
        has("натальи", "наталья");
        has("наталье", "наталья");
        has("белого", "белый");
        has("толстого", "толстой");
        has("белому", "белый");
        has("белой", "белая");
        has("петра", "петр");
        has("павла", "павел");
        has("льва", "лев");
        has("диспетчера", "диспетчер");
        assert_eq!(nominatives("ivanov"), vec!["ivanov"], "латиница не склоняется");
        assert_eq!(nominatives("петренко")[0], "петренко", "само слово — первым");
    }

    #[test]
    fn diminutives_and_first_names() {
        assert!(name_words("васи").iter().any(|w| w == "василий"));
        assert!(name_words("саше").iter().any(|w| w == "александр") && name_words("саше").iter().any(|w| w == "александра"));
        assert!(name_words("наташу").iter().any(|w| w == "наталья"));
        assert!(is_first_name("василий") && is_first_name("вася") && is_first_name("олег") && is_first_name("наталья"));
        assert!(!is_first_name("иванов") && !is_first_name("диспетчер"));
    }

    #[test]
    fn resolves_people() {
        let idx = index();
        // Фамилия в родительном падеже: и Иванов, и Иванова.
        assert_eq!(labels(&idx.resolve(&mention(&["иванова"]))), ["Иванов Иван Петрович", "Иванова Анна Сергеевна"]);
        // Творительный женской фамилии — только Иванова.
        assert_eq!(labels(&idx.resolve(&mention(&["ивановой"]))), ["Иванова Анна Сергеевна"]);
        // Уменьшительное имя: все Василии, но не Васильев; один человек с двумя адресами — один раз.
        let vasya = idx.resolve(&mention(&["васи"]));
        assert_eq!(labels(&vasya), ["Петров Василий Ильич", "Смирнов Василий Олегович"]);
        assert_eq!(vasya[0].emails, ["v.petrov@x.ru", "petrov.old@y.ru"]);
        assert_eq!(vasya[0].weight, 45);
        // Имя и отчество, фамилия и имя.
        assert_eq!(labels(&idx.resolve(&mention(&["ивана", "петровича"]))), ["Иванов Иван Петрович"]);
        assert_eq!(labels(&idx.resolve(&mention(&["петрова", "василия"]))), ["Петров Василий Ильич"]);
        assert!(idx.resolve(&mention(&["иванова", "василия"])).is_empty());
        // ё и е равнозначны: «Петра» находит «Пётр».
        assert_eq!(labels(&idx.resolve(&mention(&["петра"]))), ["Васильев Пётр Александрович"]);
        // Точных совпадений нет — по началу слова.
        assert_eq!(labels(&idx.resolve(&mention(&["диспетчера"]))), ["Диспетчерская служба"]);
        // Прилагательная фамилия.
        assert_eq!(labels(&idx.resolve(&mention(&["белого"]))), ["Белый Александр Игоревич"]);
        // По слову адреса; человек без имени называется адресом.
        assert_eq!(labels(&idx.resolve(&mention(&["ivanov"]))), ["Иванов Иван Петрович"]);
        assert_eq!(labels(&idx.resolve(&mention(&["noreply"]))), ["noreply@service.com"]);
        assert!(idx.resolve(&mention(&["поставщика"])).is_empty());
        assert!(idx.resolve(&[]).is_empty());
        assert!(idx.has_name_word("иванов") && idx.has_name_word("петр") && !idx.has_name_word("иванову"));
    }
}
