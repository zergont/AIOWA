//! Даты и периоды по-русски в запросе на обычном языке (`docs/ai-layer-design.md` § 5.2):
//! «вчера», «на прошлой неделе», «за октябрь», «летом 2024», «с 1 по 15 октября», «в 3 квартале»,
//! «перед Новым годом». Считает код, не модель; «сегодня» передаётся снаружи, поэтому тесты не
//! зависят от календаря.
//!
//! На входе — слова запроса в нижнем регистре с ё → е и без знаков по краям. Период без года
//! («в октябре», «летом», «15 марта») — ближайший прошедший или текущий.

use jiff::civil::Date;
use jiff::ToSpan;

/// Период по датам получения: начало включительно, конец не включительно; `None` — без границы.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    pub start: Option<Date>,
    pub end: Option<Date>,
}

/// Отрезок календаря: начало включительно, конец не включительно.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Unit {
    start: Date,
    end: Date,
    /// Год не назван, а выведен («в октябре», «15 марта»): в диапазоне его можно сдвинуть.
    floating: bool,
}

const MONTHS: [(&str, &[&str]); 12] = [
    ("январь", &["январь", "января", "январе", "январю", "январем"]),
    ("февраль", &["февраль", "февраля", "феврале", "февралю", "февралем"]),
    ("март", &["март", "марта", "марте", "марту", "мартом"]),
    ("апрель", &["апрель", "апреля", "апреле", "апрелю", "апрелем"]),
    ("май", &["май", "мая", "мае", "маю", "маем"]),
    ("июнь", &["июнь", "июня", "июне", "июню", "июнем"]),
    ("июль", &["июль", "июля", "июле", "июлю", "июлем"]),
    ("август", &["август", "августа", "августе", "августу", "августом"]),
    ("сентябрь", &["сентябрь", "сентября", "сентябре", "сентябрю", "сентябрем"]),
    ("октябрь", &["октябрь", "октября", "октябре", "октябрю", "октябрем"]),
    ("ноябрь", &["ноябрь", "ноября", "ноябре", "ноябрю", "ноябрем"]),
    ("декабрь", &["декабрь", "декабря", "декабре", "декабрю", "декабрем"]),
];

/// Месяцы в родительном падеже: «12 ноября 2026».
pub const MONTHS_GENITIVE: [&str; 12] = ["января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа", "сентября", "октября", "ноября", "декабря"];
/// Месяцы в именительном падеже: «ноябрь 2026».
pub const MONTHS_NOMINATIVE: [&str; 12] = ["январь", "февраль", "март", "апрель", "май", "июнь", "июль", "август", "сентябрь", "октябрь", "ноябрь", "декабрь"];

const WEEKDAYS: [&[&str]; 7] = [
    &["понедельник", "понедельника"],
    &["вторник", "вторника"],
    &["среда", "среду", "среды"],
    &["четверг", "четверга"],
    &["пятница", "пятницу", "пятницы"],
    &["суббота", "субботу", "субботы"],
    &["воскресенье", "воскресенья"],
];

const THIS: &[&str] = &["этой", "эту", "эта", "этом", "этот", "этого", "этим", "это", "текущей", "текущую", "текущая", "текущем", "текущий", "текущего"];
const LAST: &[&str] = &["прошлой", "прошлую", "прошлая", "прошлом", "прошлый", "прошлого", "прошлым", "прошлое", "предыдущей", "предыдущую", "предыдущем", "предыдущий", "предыдущего"];
const BEFORE_LAST: &[&str] = &["позапрошлой", "позапрошлую", "позапрошлом", "позапрошлый", "позапрошлого"];
const WEEK: &[&str] = &["неделе", "неделю", "недели", "неделя"];
const MONTH: &[&str] = &["месяце", "месяц", "месяца"];
const YEAR: &[&str] = &["году", "год", "года"];
const YEAR_MARK: &[&str] = &["году", "год", "года", "г", "гг"];
const QUARTER: &[&str] = &["квартал", "квартале", "квартала"];
const HALF: &[&str] = &["полугодии", "полугодие", "полугодия"];

fn ymd(y: i16, m: i8, d: i8) -> Option<Date> {
    Date::new(y, m, d).ok()
}

fn add_days(d: Date, n: i64) -> Option<Date> {
    d.checked_add(n.days()).ok()
}

/// Первое число месяца через `n` месяцев от месяца даты (n может быть отрицательным).
fn add_months(d: Date, n: i32) -> Option<Date> {
    let total = d.year() as i32 * 12 + (d.month() as i32 - 1) + n;
    ymd(i16::try_from(total.div_euclid(12)).ok()?, (total.rem_euclid(12) + 1) as i8, 1)
}

fn day_unit(d: Date) -> Option<Unit> {
    Some(Unit { start: d, end: d.tomorrow().ok()?, floating: false })
}

fn month_unit(y: i16, m: i8) -> Option<Unit> {
    let start = ymd(y, m, 1)?;
    Some(Unit { start, end: add_months(start, 1)?, floating: false })
}

fn months_unit(y: i16, m: i8, count: i32) -> Option<Unit> {
    let start = ymd(y, m, 1)?;
    Some(Unit { start, end: add_months(start, count)?, floating: false })
}

fn year_unit(y: i16) -> Option<Unit> {
    Some(Unit { start: ymd(y, 1, 1)?, end: ymd(y.checked_add(1)?, 1, 1)?, floating: false })
}

/// Неделя с понедельника, в которую попадает дата.
fn week_unit(d: Date) -> Option<Unit> {
    let start = add_days(d, -(d.weekday().to_monday_zero_offset() as i64))?;
    Some(Unit { start, end: add_days(start, 7)?, floating: false })
}

/// Последние `days` дней до сегодня включительно.
fn last_days(today: Date, days: i64) -> Option<Unit> {
    Some(Unit { start: add_days(today, -days)?, end: add_days(today, 1)?, floating: false })
}

fn floating(u: Unit) -> Unit {
    Unit { floating: true, ..u }
}

/// Тот же отрезок через `years` лет.
fn shift_year(u: Unit, years: i16) -> Option<Unit> {
    Some(Unit { start: ymd(u.start.year() + years, u.start.month(), u.start.day())?, end: ymd(u.end.year() + years, u.end.month(), u.end.day())?, floating: u.floating })
}

fn is_day(u: &Unit) -> bool {
    u.start.tomorrow().ok() == Some(u.end)
}

/// Год из слова: «2024», «2024г», «2024-го».
fn year_of(w: &str) -> Option<i16> {
    let digits = w.strip_suffix("-го").or_else(|| w.strip_suffix("г")).unwrap_or(w);
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i16 = digits.parse().ok()?;
    (1990..=2099).contains(&y).then_some(y)
}

/// День месяца из слова: «15», «15-го», «1-е».
fn day_of(w: &str) -> Option<i8> {
    let digits = ["-го", "-е", "-ое", "го"].iter().find_map(|s| w.strip_suffix(s)).unwrap_or(w);
    if digits.is_empty() || digits.len() > 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let d: i8 = digits.parse().ok()?;
    (1..=31).contains(&d).then_some(d)
}

fn month_of(w: &str) -> Option<i8> {
    MONTHS.iter().position(|(_, forms)| forms.contains(&w)).map(|i| i as i8 + 1)
}

fn small_number(w: &str) -> Option<i64> {
    if let Ok(n) = w.parse::<i64>() {
        return (1..=999).contains(&n).then_some(n);
    }
    Some(match w {
        "один" | "одну" | "одного" => 1,
        "два" | "две" | "двух" | "пару" => 2,
        "три" | "трех" => 3,
        "четыре" | "четырех" => 4,
        "пять" | "пяти" => 5,
        "шесть" | "шести" => 6,
        "семь" | "семи" => 7,
        "десять" | "десяти" => 10,
        _ => return None,
    })
}

/// Год после периода: «2025», «2025 года», «прошлого года», «этого года». Сколько слов и год.
fn trailing_year(w: &[&str], today: Date) -> Option<(usize, i16)> {
    if let Some(y) = w.first().and_then(|x| year_of(x)) {
        let mark = w.get(1).is_some_and(|x| YEAR_MARK.contains(x));
        return Some((1 + mark as usize, y));
    }
    let (a, b) = (w.first()?, w.get(1)?);
    if !YEAR.contains(b) {
        return None;
    }
    if LAST.contains(a) {
        Some((2, today.year() - 1))
    } else if THIS.contains(a) {
        Some((2, today.year()))
    } else if BEFORE_LAST.contains(a) {
        Some((2, today.year() - 2))
    } else {
        None
    }
}

/// Числовая дата одним словом: `15.10.2026`, `15.10.26`, `15.10`, `2026-10-15`, `10.2026`, `2026-10`.
fn numeric(w: &str, today: Date, anchored: bool) -> Option<Unit> {
    let dotted = w.contains('.');
    let parts: Vec<&str> = if dotted { w.split('.').collect() } else { w.split('-').collect() };
    if parts.len() < 2 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let n: Vec<i32> = parts.iter().map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let full_year = |y: i32, len: usize| -> Option<i16> {
        let y = if len == 2 { y + 2000 } else { y };
        (len == 2 || len == 4).then_some(())?;
        (1990..=2099).contains(&y).then_some(y as i16)
    };
    match (dotted, n.as_slice()) {
        (true, [d, m, y]) => day_unit(ymd(full_year(*y, parts[2].len())?, i8::try_from(*m).ok()?, i8::try_from(*d).ok()?)?),
        (true, [a, b]) if parts[1].len() == 4 => month_unit(full_year(*b, 4)?, i8::try_from(*a).ok()?),
        // «15.10» — день и месяц: только двумя цифрами каждое или после предлога («3.5» — не дата).
        (true, [d, m]) if anchored || (parts[0].len() == 2 && parts[1].len() == 2) => {
            let (d, m) = (i8::try_from(*d).ok()?, i8::try_from(*m).ok()?);
            let this = ymd(today.year(), m, d)?;
            day_unit(if this <= today { this } else { ymd(today.year() - 1, m, d)? }).map(floating)
        }
        (false, [y, m, d]) if parts[0].len() == 4 => day_unit(ymd(full_year(*y, 4)?, i8::try_from(*m).ok()?, i8::try_from(*d).ok()?)?),
        (false, [y, m]) if parts[0].len() == 4 && parts[1].len() == 2 => month_unit(full_year(*y, 4)?, i8::try_from(*m).ok()?),
        _ => None,
    }
}

/// Номер квартала или полугодия из слова.
fn ordinal(w: &str) -> Option<i8> {
    Some(match w {
        "1" | "1-й" | "1-м" | "1-го" | "1й" | "i" | "первый" | "первом" | "первого" | "первое" => 1,
        "2" | "2-й" | "2-м" | "2-го" | "2й" | "ii" | "второй" | "втором" | "второго" | "второе" => 2,
        "3" | "3-й" | "3-м" | "3-го" | "3й" | "iii" | "третий" | "третьем" | "третьего" => 3,
        "4" | "4-й" | "4-м" | "4-го" | "4й" | "iv" | "четвертый" | "четвертом" | "четвертого" => 4,
        _ => return None,
    })
}

/// Время года: первый месяц и слово.
fn season_of(w: &str) -> Option<(i8, bool)> {
    // (первый месяц, слово стоит в творительном падеже — «летом» понятно и без предлога)
    Some(match w {
        "весной" => (3, true),
        "весна" | "весны" | "весну" => (3, false),
        "летом" => (6, true),
        "лето" | "лета" => (6, false),
        "осенью" => (9, true),
        "осень" | "осени" => (9, false),
        "зимой" => (12, true),
        "зима" | "зимы" | "зиму" => (12, false),
        _ => return None,
    })
}

/// Время года с первым месяцем `first` (12 — зима, с декабря) по году, на который оно приходится:
/// зима 2026 — декабрь 2025 … февраль 2026.
fn season_unit(first: i8, year: i16) -> Option<Unit> {
    if first == 12 { months_unit(year - 1, 12, 3) } else { months_unit(year, first, 3) }
}

/// Последний отрезок из ряда `make(год)`, который уже начался (`passed` — уже закончился).
fn latest(today: Date, passed: bool, make: impl Fn(i16) -> Option<Unit>) -> Option<Unit> {
    for y in [today.year() + 1, today.year(), today.year() - 1] {
        if let Some(u) = make(y)
            && (if passed { u.end <= today } else { u.start <= today })
        {
            return Some(floating(u));
        }
    }
    None
}

/// Отрезок календаря в начале `w`. `prep` — предлог перед ним («за», «в», «с», «до») или пусто:
/// после предлога годятся и «2024», «пятницу», а после «за» — «неделю», «год», которые без
/// предлога — обычные слова.
fn unit(w: &[&str], today: Date, prep: &str) -> Option<(usize, Unit)> {
    let anchored = !prep.is_empty();
    let w0 = *w.first()?;
    let w1 = w.get(1).copied().unwrap_or("");

    // Дни.
    match w0 {
        "сегодня" => return Some((1, day_unit(today)?)),
        "вчера" => return Some((1, day_unit(add_days(today, -1)?)?)),
        "позавчера" => return Some((1, day_unit(add_days(today, -2)?)?)),
        "недавно" => return Some((1, last_days(today, 7)?)),
        "днях" if prep == "на" => return Some((1, last_days(today, 7)?)),
        "неделе" if prep == "на" => return Some((1, week_unit(today)?)),
        _ => {}
    }
    if w0 == "последнее" && w1 == "время" {
        return Some((2, last_days(today, 30)?));
    }

    // «в начале октября», «в конце прошлого месяца», «в середине года».
    let part = match w0 {
        "начале" | "начало" | "начала" => Some(0),
        "середине" | "середина" | "середины" => Some(1),
        "конце" | "конец" | "конца" => Some(2),
        _ => None,
    };
    if let Some(part) = part {
        let rest = &w[1..];
        // «месяца» и «года» без уточнения — текущие.
        let (n, whole) = if rest.first().is_some_and(|x| *x == "месяца") {
            (1, month_unit(today.year(), today.month())?)
        } else if rest.first().is_some_and(|x| *x == "года") {
            (1, year_unit(today.year())?)
        } else {
            unit(rest, today, "в")?
        };
        let days = (whole.end.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp().as_second() - whole.start.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp().as_second()) / 86_400;
        let cut = if (28..=31).contains(&days) {
            // Месяц — по декадам.
            match part {
                0 => Unit { start: whole.start, end: add_days(whole.start, 10)?, floating: whole.floating },
                1 => Unit { start: add_days(whole.start, 10)?, end: add_days(whole.start, 20)?, floating: whole.floating },
                _ => Unit { start: add_days(whole.start, 20)?, end: whole.end, floating: whole.floating },
            }
        } else if (365..=366).contains(&days) {
            // Год — первый квартал, середина (май — август), последний квартал.
            let y = whole.start.year();
            match part {
                0 => months_unit(y, 1, 3)?,
                1 => months_unit(y, 5, 4)?,
                _ => months_unit(y, 10, 3)?,
            }
        } else {
            return None;
        };
        return Some((1 + n, cut));
    }

    // Недели, месяцы, годы со словами «этот», «прошлый», «позапрошлый».
    let shift = if THIS.contains(&w0) {
        Some(0)
    } else if LAST.contains(&w0) {
        Some(1)
    } else if BEFORE_LAST.contains(&w0) {
        Some(2)
    } else {
        None
    };
    if let Some(back) = shift {
        if WEEK.contains(&w1) {
            return Some((2, week_unit(add_days(today, -7 * back)?)?));
        }
        if MONTH.contains(&w1) {
            let m = add_months(today, -(back as i32))?;
            return Some((2, month_unit(m.year(), m.month())?));
        }
        if YEAR.contains(&w1) {
            return Some((2, year_unit(today.year() - back as i16)?));
        }
        if QUARTER.contains(&w1) {
            let this = add_months(ymd(today.year(), ((today.month() - 1) / 3) * 3 + 1, 1)?, -3 * back as i32)?;
            return Some((2, months_unit(this.year(), this.month(), 3)?));
        }
        // «прошлым летом», «этой зимой».
        if let Some((first, _)) = season_of(w1) {
            return Some((2, latest(today, back > 0, |y| season_unit(first, y))?));
        }
        // «в прошлую пятницу», «в эту среду».
        if anchored && let Some(wd) = WEEKDAYS.iter().position(|f| f.contains(&w1)) {
            let week = week_unit(add_days(today, -7 * back)?)?;
            return Some((2, day_unit(add_days(week.start, wd as i64)?)?));
        }
    }

    // «за последние 3 дня», «за 2 недели», «за последний месяц».
    {
        let (skip, rest) = if matches!(w0, "последние" | "последних" | "последнюю" | "последний" | "последнего") { (1, &w[1..]) } else { (0, w) };
        if skip == 1 || prep == "за" {
            let (count, cw, noun) = match rest.first().and_then(|x| small_number(x)) {
                Some(n) => (n, 1, rest.get(1).copied().unwrap_or("")),
                None => (1, 0, rest.first().copied().unwrap_or("")),
            };
            // «за год» и «за месяц» без числа понятны; «за 2024» — это год, не «2024 дня».
            let days = match noun {
                "день" | "дня" | "дней" | "сутки" | "суток" if cw == 1 => Some(count),
                "неделю" | "недели" | "недель" => Some(count * 7),
                "месяц" | "месяца" | "месяцев" => Some(count * 30),
                "год" | "года" | "лет" if w.first().and_then(|x| year_of(x)).is_none() => Some(count * 365),
                "полгода" if cw == 0 => Some(183),
                _ => None,
            };
            if let Some(days) = days {
                return Some((skip + cw + 1, last_days(today, days)?));
            }
        }
    }

    // «15 октября», «15 октября 2025 года».
    if let (Some(d), Some(m)) = (day_of(w0), month_of(w1)) {
        return match trailing_year(&w[2..], today) {
            Some((n, y)) => Some((2 + n, day_unit(ymd(y, m, d)?)?)),
            None => {
                let this = ymd(today.year(), m, d)?;
                Some((2, floating(day_unit(if this <= today { this } else { ymd(today.year() - 1, m, d)? })?)))
            }
        };
    }

    // «октябрь», «в октябре 2025», «октябрь прошлого года».
    if let Some(m) = month_of(w0) {
        return match trailing_year(&w[1..], today) {
            Some((n, y)) => Some((1 + n, month_unit(y, m)?)),
            None => Some((1, latest(today, false, |y| month_unit(y, m))?)),
        };
    }

    // Числовые даты.
    if let Some(u) = numeric(w0, today, anchored) {
        return Some((1, u));
    }

    // «2024 год», «в 2024».
    if let Some(y) = year_of(w0) {
        if YEAR_MARK.contains(&w1) {
            return Some((2, year_unit(y)?));
        }
        if anchored || w0.ends_with('г') {
            return Some((1, year_unit(y)?));
        }
    }

    // «летом», «летом 2024», «зима 2025».
    if let Some((first, bare_ok)) = season_of(w0) {
        return match trailing_year(&w[1..], today) {
            Some((n, y)) => Some((1 + n, season_unit(first, y)?)),
            None if bare_ok || anchored => Some((1, latest(today, false, |y| season_unit(first, y))?)),
            None => None,
        };
    }

    // «3 квартал», «в первом квартале 2026», «во втором полугодии».
    if let Some(n) = ordinal(w0) {
        if QUARTER.contains(&w1) {
            let first = (n - 1) * 3 + 1;
            return match trailing_year(&w[2..], today) {
                Some((k, y)) => Some((2 + k, months_unit(y, first, 3)?)),
                None => Some((2, latest(today, false, |y| months_unit(y, first, 3))?)),
            };
        }
        if HALF.contains(&w1) && n <= 2 {
            let first = (n - 1) * 6 + 1;
            return match trailing_year(&w[2..], today) {
                Some((k, y)) => Some((2 + k, months_unit(y, first, 6)?)),
                None => Some((2, latest(today, false, |y| months_unit(y, first, 6))?)),
            };
        }
    }

    // «в пятницу» — последняя прошедшая или сегодняшняя.
    if anchored && let Some(wd) = WEEKDAYS.iter().position(|f| f.contains(&w0)) {
        let back = (today.weekday().to_monday_zero_offset() as i64 - wd as i64).rem_euclid(7);
        return Some((1, day_unit(add_days(today, -back)?)?));
    }
    None
}

/// Период в начале слов `w`: сколько слов он занял и сам период. Слова — в нижнем регистре, ё → е.
pub fn parse(w: &[&str], today: Date) -> Option<(usize, Period)> {
    let w0 = *w.first()?;
    let both = |u: Unit| Period { start: Some(u.start), end: Some(u.end) };

    // «перед Новым годом», «после Нового года».
    if w.len() >= 3 && matches!(w0, "перед" | "под") && w[1] == "новым" && w[2] == "годом" {
        let u = latest(today, false, |y| Some(Unit { start: ymd(y, 12, 15)?, end: ymd(y + 1, 1, 1)?, floating: true }))?;
        return Some((3, both(u)));
    }
    if w.len() >= 3 && w0 == "после" && w[1] == "нового" && w[2] == "года" {
        let u = latest(today, false, |y| Some(Unit { start: ymd(y, 1, 1)?, end: ymd(y, 1, 21)?, floating: true }))?;
        return Some((3, both(u)));
    }

    // «с 1 по 15 октября», «с октября по декабрь», «с 01.10 до 15.10», «между 1 и 15 октября».
    let range_start = match w0 {
        "с" | "со" | "от" | "между" => Some(1),
        "начиная" if w.get(1).is_some_and(|x| matches!(*x, "с" | "со")) => Some(2),
        _ => None,
    };
    if let Some(from) = range_start {
        let joiner = |x: &str| if w0 == "между" { x == "и" } else { matches!(x, "по" | "до") };
        // Начало — одно число дня: месяц и год берутся у конца («с 1 по 15 октября»).
        if let Some(d) = w.get(from).and_then(|x| day_of(x))
            && w.get(from + 1).is_some_and(|x| joiner(x))
            && let Some((n, last)) = unit(&w[from + 2..], today, "по")
            && is_day(&last)
            && let Some(first) = ymd(last.start.year(), last.start.month(), d)
            && first <= last.start
        {
            return Some((from + 2 + n, Period { start: Some(first), end: Some(last.end) }));
        }
        if let Some((n1, first)) = unit(&w[from..], today, "с") {
            let after = from + n1;
            if w.get(after).is_some_and(|x| joiner(x))
                && let Some((n2, last)) = unit(&w[after + 1..], today, "по")
            {
                // Начало позже конца: год у одного из них выведен неверно. Конец без года можно
                // перенести на год вперёд, если это близко («с 1 октября по 15 ноября» в середине
                // ноября); иначе начало без года — годом раньше («с ноября по февраль»).
                let pair = if first.start <= last.start {
                    Some((first, last))
                } else if last.floating
                    && let Some(later) = shift_year(last, 1)
                    && later.start <= add_days(today, 60)?
                {
                    Some((first, later))
                } else if first.floating {
                    shift_year(first, -1).map(|f| (f, last))
                } else {
                    None
                };
                if let Some((first, last)) = pair
                    && first.start <= last.start
                {
                    return Some((after + 1 + n2, Period { start: Some(first.start), end: Some(last.end) }));
                }
            }
            // «с октября», «начиная с 15.10» — от начала и дальше; «между» и «от» без пары — не период.
            if matches!(w0, "с" | "со" | "начиная") {
                return Some((after, Period { start: Some(first.start), end: None }));
            }
            if w0 == "от" && is_day(&first) {
                // «письмо от 15 октября» — письмо этого дня.
                return Some((after, both(first)));
            }
        }
        return None;
    }

    match w0 {
        "после" | "позже" | "позднее" => {
            let (n, u) = unit(&w[1..], today, "после")?;
            Some((1 + n, Period { start: Some(u.end), end: None }))
        }
        "до" | "раньше" | "ранее" => {
            let (n, u) = unit(&w[1..], today, "до")?;
            Some((1 + n, Period { start: None, end: Some(u.start) }))
        }
        "по" => {
            // «по 15 октября» — включительно; «по договору» — не дата.
            let (n, u) = unit(&w[1..], today, "")?;
            Some((1 + n, Period { start: None, end: Some(u.end) }))
        }
        "за" | "в" | "во" | "на" => {
            let (n, u) = unit(&w[1..], today, if w0 == "во" { "в" } else { w0 })?;
            Some((1 + n, both(u)))
        }
        _ => {
            let (n, u) = unit(w, today, "")?;
            Some((n, both(u)))
        }
    }
}

/// Период словами для чипа: «12 ноября 2026», «октябрь 2026», «2024 год», «1–15 октября 2026»,
/// «с 6 ноября 2026», «по 31 октября 2026».
pub fn label(p: &Period) -> String {
    let day = |d: Date| format!("{} {} {}", d.day(), MONTHS_GENITIVE[(d.month() - 1) as usize], d.year());
    match (p.start, p.end) {
        (Some(s), Some(e)) => {
            let last = e.yesterday().unwrap_or(e);
            if last == s {
                day(s)
            } else if s.day() == 1 && e.day() == 1 && add_months(s, 1) == Some(e) {
                format!("{} {}", MONTHS_NOMINATIVE[(s.month() - 1) as usize], s.year())
            } else if s.day() == 1 && s.month() == 1 && e.day() == 1 && e.month() == 1 && e.year() == s.year() + 1 {
                format!("{} год", s.year())
            } else if s.year() == last.year() && s.month() == last.month() {
                format!("{}–{} {} {}", s.day(), last.day(), MONTHS_GENITIVE[(s.month() - 1) as usize], s.year())
            } else if s.year() == last.year() {
                format!("{} {} – {} {} {}", s.day(), MONTHS_GENITIVE[(s.month() - 1) as usize], last.day(), MONTHS_GENITIVE[(last.month() - 1) as usize], s.year())
            } else {
                format!("{} – {}", day(s), day(last))
            }
        }
        (Some(s), None) => format!("с {}", day(s)),
        (None, Some(e)) => format!("по {}", day(e.yesterday().unwrap_or(e))),
        (None, None) => String::new(),
    }
}

/// Период операторами поиска: `дата:2026-10`, `дата:2026-11-12`, `после:2026-10-01 до:2026-10-15`.
/// У `до:` последний день входит в период (как понимает его `query::parse`).
pub fn operators(p: &Period) -> String {
    let iso = |d: Date| format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day());
    match (p.start, p.end) {
        (Some(s), Some(e)) => {
            let last = e.yesterday().unwrap_or(e);
            if last == s {
                format!("дата:{}", iso(s))
            } else if s.day() == 1 && e.day() == 1 && add_months(s, 1) == Some(e) {
                format!("дата:{:04}-{:02}", s.year(), s.month())
            } else if s.day() == 1 && s.month() == 1 && e.day() == 1 && e.month() == 1 && e.year() == s.year() + 1 {
                format!("дата:{:04}", s.year())
            } else {
                format!("после:{} до:{}", iso(s), iso(last))
            }
        }
        (Some(s), None) => format!("после:{}", iso(s)),
        (None, Some(e)) => format!("до:{}", iso(e.yesterday().unwrap_or(e))),
        (None, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Пятница, 13 ноября 2026.
    fn today() -> Date {
        ymd(2026, 11, 13).unwrap()
    }

    fn p(text: &str) -> Option<(usize, String)> {
        let words: Vec<&str> = text.split_whitespace().collect();
        parse(&words, today()).map(|(n, period)| (n, label(&period)))
    }

    fn full(text: &str) -> String {
        let n = text.split_whitespace().count();
        match p(text) {
            Some((k, l)) if k == n => l,
            other => panic!("«{text}»: {other:?}"),
        }
    }

    #[test]
    fn days_and_weeks() {
        assert_eq!(full("сегодня"), "13 ноября 2026");
        assert_eq!(full("вчера"), "12 ноября 2026");
        assert_eq!(full("за вчера"), "12 ноября 2026");
        assert_eq!(full("позавчера"), "11 ноября 2026");
        assert_eq!(full("на этой неделе"), "9–15 ноября 2026");
        assert_eq!(full("на прошлой неделе"), "2–8 ноября 2026");
        assert_eq!(full("за прошлую неделю"), "2–8 ноября 2026");
        assert_eq!(full("на позапрошлой неделе"), "26 октября – 1 ноября 2026");
        assert_eq!(full("за неделю"), "6–13 ноября 2026");
        assert_eq!(full("за последнюю неделю"), "6–13 ноября 2026");
        assert_eq!(full("за последние 3 дня"), "10–13 ноября 2026");
        assert_eq!(full("за 2 недели"), "30 октября – 13 ноября 2026");
        assert_eq!(full("на днях"), "6–13 ноября 2026");
        assert_eq!(full("недавно"), "6–13 ноября 2026");
        assert_eq!(full("в пятницу"), "13 ноября 2026");
        assert_eq!(full("в среду"), "11 ноября 2026");
        assert_eq!(full("в субботу"), "7 ноября 2026");
        assert_eq!(full("в прошлую пятницу"), "6 ноября 2026");
        assert_eq!(full("во вторник"), "10 ноября 2026");
    }

    #[test]
    fn months_and_years() {
        assert_eq!(full("за октябрь"), "октябрь 2026");
        assert_eq!(full("в октябре"), "октябрь 2026");
        assert_eq!(full("октябрь"), "октябрь 2026");
        assert_eq!(full("в ноябре"), "ноябрь 2026");
        assert_eq!(full("в декабре"), "декабрь 2025", "месяц без года — прошедший");
        assert_eq!(full("за октябрь 2025"), "октябрь 2025");
        assert_eq!(full("в октябре 2025 года"), "октябрь 2025");
        assert_eq!(full("в марте прошлого года"), "март 2025");
        assert_eq!(full("в этом месяце"), "ноябрь 2026");
        assert_eq!(full("в прошлом месяце"), "октябрь 2026");
        assert_eq!(full("за прошлый месяц"), "октябрь 2026");
        assert_eq!(full("в позапрошлом месяце"), "сентябрь 2026");
        assert_eq!(full("за месяц"), "14 октября – 13 ноября 2026");
        assert_eq!(full("в этом году"), "2026 год");
        assert_eq!(full("в прошлом году"), "2025 год");
        assert_eq!(full("за прошлый год"), "2025 год");
        assert_eq!(full("в позапрошлом году"), "2024 год");
        assert_eq!(full("в 2024"), "2024 год");
        assert_eq!(full("в 2024 году"), "2024 год");
        assert_eq!(full("за 2024 год"), "2024 год");
        assert_eq!(full("2024 года"), "2024 год");
        assert_eq!(full("за год"), "13 ноября 2025 – 13 ноября 2026");
        assert_eq!(full("за полгода"), "14 мая – 13 ноября 2026");
        assert_eq!(full("за последние 2 месяца"), "14 сентября – 13 ноября 2026");
    }

    #[test]
    fn exact_dates_and_ranges() {
        assert_eq!(full("15 октября"), "15 октября 2026");
        assert_eq!(full("15 декабря"), "15 декабря 2025", "будущий день без года — прошлогодний");
        assert_eq!(full("15 октября 2025"), "15 октября 2025");
        assert_eq!(full("15-го октября 2025 года"), "15 октября 2025");
        assert_eq!(full("15.10.2026"), "15 октября 2026");
        assert_eq!(full("15.10.25"), "15 октября 2025");
        assert_eq!(full("15.10"), "15 октября 2026");
        assert_eq!(full("2026-10-15"), "15 октября 2026");
        assert_eq!(full("10.2026"), "октябрь 2026");
        assert_eq!(full("от 15 октября"), "15 октября 2026");
        assert_eq!(full("с 1 по 15 октября"), "1–15 октября 2026");
        assert_eq!(full("с 1 октября по 15 ноября"), "1 октября – 15 ноября 2026");
        assert_eq!(full("с 01.10 по 15.10"), "1–15 октября 2026");
        assert_eq!(full("с октября по декабрь 2025"), "1 октября – 31 декабря 2025");
        assert_eq!(full("с ноября по февраль"), "1 ноября 2025 – 28 февраля 2026");
        assert_eq!(full("между 1 и 15 октября"), "1–15 октября 2026");
        assert_eq!(full("с октября"), "с 1 октября 2026");
        assert_eq!(full("начиная с 15.10"), "с 15 октября 2026");
        assert_eq!(full("после 15 октября"), "с 16 октября 2026");
        assert_eq!(full("до 1 ноября"), "по 31 октября 2026");
        assert_eq!(full("до октября"), "по 30 сентября 2026");
        assert_eq!(full("по 15 октября"), "по 15 октября 2026");
        assert_eq!(full("после 2024"), "с 1 января 2025");
    }

    #[test]
    fn seasons_quarters_and_parts() {
        assert_eq!(full("летом"), "1 июня – 31 августа 2026");
        assert_eq!(full("летом 2024"), "1 июня – 31 августа 2024");
        assert_eq!(full("прошлым летом"), "1 июня – 31 августа 2026");
        assert_eq!(full("зимой"), "1 декабря 2025 – 28 февраля 2026");
        assert_eq!(full("этой осенью"), "1 сентября – 30 ноября 2026");
        assert_eq!(full("прошлой осенью"), "1 сентября – 30 ноября 2025");
        assert_eq!(full("весной 2025 года"), "1 марта – 31 мая 2025");
        assert_eq!(full("в 3 квартале"), "1 июля – 30 сентября 2026");
        assert_eq!(full("в первом квартале 2026"), "1 января – 31 марта 2026");
        assert_eq!(full("за 4 квартал"), "1 октября – 31 декабря 2026");
        assert_eq!(full("в прошлом квартале"), "1 июля – 30 сентября 2026");
        assert_eq!(full("во втором полугодии"), "1 июля – 31 декабря 2026");
        assert_eq!(full("в первом полугодии 2025 года"), "1 января – 30 июня 2025");
        assert_eq!(full("в начале октября"), "1–10 октября 2026");
        assert_eq!(full("в середине октября"), "11–20 октября 2026");
        assert_eq!(full("в конце октября"), "21–31 октября 2026");
        assert_eq!(full("в конце прошлого месяца"), "21–31 октября 2026");
        assert_eq!(full("в начале месяца"), "1–10 ноября 2026");
        assert_eq!(full("в начале года"), "1 января – 31 марта 2026");
        assert_eq!(full("в конце прошлого года"), "1 октября – 31 декабря 2025");
        assert_eq!(full("перед новым годом"), "15–31 декабря 2025");
        assert_eq!(full("после нового года"), "1–20 января 2026");
    }

    #[test]
    fn not_dates() {
        for text in ["договор", "по договору", "от иванова", "с вложением", "в папке", "3.5", "2024", "неделю", "пятницу", "год", "на складе", "к оплате", "мир", "лето", "маркетинг", "между нами", "с уважением", "12345", "1 квартира", "в среде", "раз в месяц", "в месяц", "на 5 дней", "к пятнице"] {
            assert_eq!(p(text), None, "«{text}»");
        }
        // Период занимает только свои слова.
        assert_eq!(p("за октябрь про ремонт"), Some((2, "октябрь 2026".into())));
        assert_eq!(p("вчера от иванова"), Some((1, "12 ноября 2026".into())));
        assert_eq!(p("в октябре 2025 договор"), Some((3, "октябрь 2025".into())));
        assert_eq!(p("15 октября 2020 штук"), Some((3, "15 октября 2020".into())));
    }

    #[test]
    fn operator_form() {
        let op = |text: &str| {
            let words: Vec<&str> = text.split_whitespace().collect();
            operators(&parse(&words, today()).unwrap().1)
        };
        assert_eq!(op("за октябрь"), "дата:2026-10");
        assert_eq!(op("вчера"), "дата:2026-11-12");
        assert_eq!(op("в 2024 году"), "дата:2024");
        assert_eq!(op("с 1 по 15 октября"), "после:2026-10-01 до:2026-10-15");
        assert_eq!(op("после 15 октября"), "после:2026-10-16");
        assert_eq!(op("до 1 ноября"), "до:2026-10-31");
        assert_eq!(op("летом 2024"), "после:2024-06-01 до:2024-08-31");
    }
}
