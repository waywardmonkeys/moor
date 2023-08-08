use anyhow::bail;
use moor_value::var::error::Error;
use thiserror::Error;

use moor_value::var::objid::Objid;

use crate::model::objects::ObjAttr;
use crate::model::verbs::Vid;

pub mod r#match;
pub mod objects;
pub mod permissions;
pub mod props;
pub mod verbs;
pub mod world_state;

// TODO ditch the bulk of these error codes and replace with MOO Error, except for the match
// functions. We need to be able to translate back to MOO error codes and have them raised in the
// VM, without a lot of hassle.
#[derive(Error, Debug, Eq, PartialEq)]
pub enum ObjectError {
    #[error("Object not found: {0}")]
    ObjectNotFound(Objid),
    #[error("Object already exists: {0}")]
    ObjectAlreadyExists(Objid),
    #[error("Could not set/get object attribute; {0} on #{1}")]
    ObjectAttributeError(ObjAttr, Objid),
    #[error("Recursive move detected: {0} -> {1}")]
    RecursiveMove(Objid, Objid),

    #[error("Object permission denied")]
    ObjectPermissionDenied,

    #[error("Property not found: {0}.{1}")]
    PropertyNotFound(Objid, String),
    #[error("Property permission denied")]
    PropertyPermissionDenied,
    #[error("Property definition not found: {0}.{1}")]
    PropertyDefinitionNotFound(Objid, String),
    #[error("Duplicate property definition: {0}.{1}")]
    DuplicatePropertyDefinition(Objid, String),

    #[error("Verb not found: {0}:{1}")]
    VerbNotFound(Objid, String),
    #[error("Verb definition not {0:?}")]
    InvalidVerb(Vid),

    #[error("Invalid verb, decode error: {0}:{1}")]
    VerbDecodeError(Objid, String),
    #[error("Verb permission denied")]
    VerbPermissionDenied,
    #[error("Verb already exists: {0}:{1}")]
    DuplicateVerb(Objid, String),

    #[error("Failed object match: {0}")]
    FailedMatch(String),
    #[error("Ambiguous object match: {0}")]
    AmbiguousMatch(String),

    // Catch-alls for system level object DB errors.
    #[error("Object DB error for {0}: {1}")]
    ObjectDbError(Objid, String),
    #[error("Object DB error for {0}.{1}: {2}")]
    PropertyDbError(Objid, String, String),
    #[error("Object DB error for {0}:{1}: {2}")]
    VerbDbError(Objid, String, String),
}

impl ObjectError {
    pub fn to_error_code(&self) -> Result<Error, anyhow::Error> {
        match self {
            ObjectError::ObjectNotFound(_) => Ok(Error::E_INVARG),
            ObjectError::ObjectPermissionDenied => Ok(Error::E_PERM),
            ObjectError::RecursiveMove(_, _) => Ok(Error::E_RECMOVE),
            ObjectError::VerbNotFound(_, _) => Ok(Error::E_VERBNF),
            ObjectError::VerbPermissionDenied => Ok(Error::E_PERM),
            ObjectError::InvalidVerb(_) => Ok(Error::E_VERBNF),
            ObjectError::PropertyNotFound(_, _) => Ok(Error::E_PROPNF),
            ObjectError::PropertyPermissionDenied => Ok(Error::E_PERM),
            ObjectError::PropertyDefinitionNotFound(_, _) => Ok(Error::E_PROPNF),
            _ => {
                bail!("Unhandled error code: {:?}", self);
            }
        }
    }
}
