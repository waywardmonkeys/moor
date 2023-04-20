use async_trait::async_trait;

use magic_crypt::{new_magic_crypt, MagicCryptTrait};
use rand::distributions::Alphanumeric;
use rand::Rng;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bf_declare;
use crate::compiler::builtins::offset_for_builtin;
use crate::db::state::WorldState;
use crate::model::var::Error::{E_INVARG, E_TYPE};
use crate::model::var::Var;
use crate::server::Sessions;
use crate::vm::activation::Activation;
use crate::vm::execute::{BfFunction, VM};

fn strsub(subject: &str, what: &str, with: &str, case_matters: bool) -> String {
    let mut result = String::new();
    let mut source = subject;

    if what.is_empty() || with.is_empty() {
        return subject.to_string();
    }

    while let Some(index) = if case_matters {
        source.find(what)
    } else {
        source.to_lowercase().find(&what.to_lowercase())
    } {
        result.push_str(&source[..index]);
        result.push_str(with);
        let next = index + what.len();
        source = &source[next..];
    }

    result.push_str(source);

    result
}

//Function: str strsub (str subject, str what, str with [, case-matters])
async fn bf_strsub(
    _ws: &mut dyn WorldState,
    _frame: &mut Activation,
    _sess: Arc<Mutex<dyn Sessions>>,
    args: Vec<Var>,
) -> Result<Var, anyhow::Error> {
    let case_matters = if args.len() == 3 {
        false
    } else if args.len() == 4 {
        let Some(Var::Int(case_matters)) = args.get(3) else {
            return Ok(Var::Err(E_TYPE));
        };
        *case_matters == 1
    } else {
        return Ok(Var::Err(E_INVARG));
    };
    let (subject, what, with) = (&args[0], &args[1], &args[2]);
    match (subject, what, with) {
        (Var::Str(subject), Var::Str(what), Var::Str(with)) => {
            Ok(Var::Str(strsub(subject, what, with, case_matters)))
        }
        _ => Ok(Var::Err(E_TYPE)),
    }
}
bf_declare!(strsub, bf_strsub);

fn str_index(subject: &str, what: &str, case_matters: bool) -> i64 {
    if case_matters {
        subject.find(what).map(|i| i as i64 + 1).unwrap_or(0)
    } else {
        subject
            .to_lowercase()
            .find(&what.to_lowercase())
            .map(|i| i as i64 + 1)
            .unwrap_or(0)
    }
}

fn str_rindex(subject: &str, what: &str, case_matters: bool) -> i64 {
    if case_matters {
        subject.rfind(what).map(|i| i as i64 + 1).unwrap_or(0)
    } else {
        subject
            .to_lowercase()
            .rfind(&what.to_lowercase())
            .map(|i| i as i64 + 1)
            .unwrap_or(0)
    }
}

async fn bf_index(
    _ws: &mut dyn WorldState,
    _frame: &mut Activation,
    _sess: Arc<Mutex<dyn Sessions>>,
    args: Vec<Var>,
) -> Result<Var, anyhow::Error> {
    let case_matters = if args.len() == 2 {
        false
    } else if args.len() == 3 {
        let Some(Var::Int(case_matters)) = args.get(2) else {
            return Ok(Var::Err(E_TYPE));
        };
        *case_matters == 1
    } else {
        return Ok(Var::Err(E_INVARG));
    };

    let (subject, what) = (&args[0], &args[1]);
    match (subject, what) {
        (Var::Str(subject), Var::Str(what)) => Ok(Var::Int(str_index(subject, what, case_matters))),
        _ => Ok(Var::Err(E_TYPE)),
    }
}
bf_declare!(index, bf_index);

async fn bf_rindex(
    _ws: &mut dyn WorldState,
    _frame: &mut Activation,
    _sess: Arc<Mutex<dyn Sessions>>,
    args: Vec<Var>,
) -> Result<Var, anyhow::Error> {
    let case_matters = if args.len() == 2 {
        false
    } else if args.len() == 3 {
        let Some(Var::Int(case_matters)) = args.get(2) else {
            return Ok(Var::Err(E_TYPE));
        };
        *case_matters == 1
    } else {
        return Ok(Var::Err(E_INVARG));
    };

    let (subject, what) = (&args[0], &args[1]);
    match (subject, what) {
        (Var::Str(subject), Var::Str(what)) => {
            Ok(Var::Int(str_rindex(subject, what, case_matters)))
        }
        _ => Ok(Var::Err(E_TYPE)),
    }
}
bf_declare!(rindex, bf_rindex);

async fn bf_strcmp(
    _ws: &mut dyn WorldState,
    _frame: &mut Activation,
    _sess: Arc<Mutex<dyn Sessions>>,
    args: Vec<Var>,
) -> Result<Var, anyhow::Error> {
    if args.len() != 2 {
        return Ok(Var::Err(E_INVARG));
    }
    let (str1, str2) = (&args[0], &args[1]);
    match (str1, str2) {
        (Var::Str(str1), Var::Str(str2)) => Ok(Var::Int(str1.cmp(str2) as i64)),
        _ => Ok(Var::Err(E_TYPE)),
    }
}
bf_declare!(strcmp, bf_strcmp);

/*
str crypt (str text [, str salt])

Encrypts the given text using the standard UNIX encryption method. If provided, salt should be a
string at least two characters long, the first two characters of which will be used as the extra
encryption "salt" in the algorithm. If salt is not provided, a random pair of characters is used.
 In any case, the salt used is also returned as the first two characters of the resulting encrypted
 string.

`crypt` is DES encryption, so that's what we do.
 */
fn des_crypt(text: &str, salt: &str) -> String {
    let mc = new_magic_crypt!(salt);
    let crypted = mc.encrypt_str_to_bytes(text);
    crypted.iter().map(|i| char::from(*i)).collect()
}

async fn bf_crypt(
    _ws: &mut dyn WorldState,
    _frame: &mut Activation,
    _sess: Arc<Mutex<dyn Sessions>>,
    args: Vec<Var>,
) -> Result<Var, anyhow::Error> {
    if args.is_empty() || args.len() > 2 {
        return Ok(Var::Err(E_INVARG));
    }
    let salt = if args.len() == 1 {
        let mut rng = rand::thread_rng();
        let mut salt = String::new();

        salt.push(char::from(rng.sample(Alphanumeric)));
        salt.push(char::from(rng.sample(Alphanumeric)));
        salt
    } else {
        let Var::Str(salt) = &args[1] else {
            return Ok(Var::Err(E_TYPE));
        };
        salt.clone()
    };
    if let Var::Str(text) = &args[0] {
        Ok(Var::Str(des_crypt(text, salt.as_str())))
    } else {
        Ok(Var::Err(E_TYPE))
    }
}
bf_declare!(crypt, bf_crypt);

async fn bf_string_hash(
    _ws: &mut dyn WorldState,
    _frame: &mut Activation,
    _sess: Arc<Mutex<dyn Sessions>>,
    args: Vec<Var>,
) -> Result<Var, anyhow::Error> {
    if args.len() != 1 {
        return Ok(Var::Err(E_INVARG));
    }
    match &args[0] {
        Var::Str(s) => {
            let hash_digest = md5::compute(s.as_bytes());
            Ok(Var::Str(format!("{:x}", hash_digest)))
        }
        _ => Ok(Var::Err(E_INVARG)),
    }
}
bf_declare!(string_hash, bf_string_hash);

async fn bf_binary_hash(
    _ws: &mut dyn WorldState,
    _frame: &mut Activation,
    _sess: Arc<Mutex<dyn Sessions>>,
    _args: Vec<Var>,
) -> Result<Var, anyhow::Error> {
    unimplemented!("binary_hash")
}
bf_declare!(binary_hash, bf_binary_hash);

impl VM {
    pub(crate) fn register_bf_strings(&mut self) -> Result<(), anyhow::Error> {
        self.bf_funcs[offset_for_builtin("strsub")] = Arc::new(Box::new(BfStrsub {}));
        self.bf_funcs[offset_for_builtin("index")] = Arc::new(Box::new(BfIndex {}));
        self.bf_funcs[offset_for_builtin("rindex")] = Arc::new(Box::new(BfRindex {}));
        self.bf_funcs[offset_for_builtin("strcmp")] = Arc::new(Box::new(BfStrcmp {}));
        self.bf_funcs[offset_for_builtin("crypt")] = Arc::new(Box::new(BfCrypt {}));
        self.bf_funcs[offset_for_builtin("string_hash")] = Arc::new(Box::new(BfStringHash {}));
        self.bf_funcs[offset_for_builtin("binary_hash")] = Arc::new(Box::new(BfBinaryHash {}));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::vm::bf_strings::strsub;

    #[test]
    fn test_strsub_case_insensitive_substitution() {
        let subject = "foo bar baz";
        let expected = "fizz bar baz";
        assert_eq!(strsub(subject, "foo", "fizz", false), expected);
    }

    #[test]
    fn test_strsub_case_sensitive_substitution() {
        let subject = "foo bar baz";
        let expected = "foo bar fizz";
        assert_eq!(strsub(subject, "baz", "fizz", true), expected);
    }

    #[test]
    fn test_strsub_empty_subject() {
        let subject = "";
        let expected = "";
        assert_eq!(strsub(subject, "foo", "fizz", false), expected);
    }

    #[test]
    fn test_strsub_empty_what() {
        let subject = "foo bar baz";
        let expected = "foo bar baz";
        assert_eq!(strsub(subject, "", "fizz", false), expected);
    }

    #[test]
    fn test_strsub_empty_with() {
        let subject = "foo bar baz";
        let expected = "foo bar baz";
        assert_eq!(strsub(subject, "foo", "", false), expected);
    }

    #[test]
    fn test_strsub_multiple_occurrences() {
        let subject = "foo foo foo";
        let expected = "fizz fizz fizz";
        assert_eq!(strsub(subject, "foo", "fizz", false), expected);
    }

    #[test]
    fn test_strsub_no_occurrences() {
        let subject = "foo bar baz";
        let expected = "foo bar baz";
        assert_eq!(strsub(subject, "fizz", "buzz", false), expected);
    }
}
