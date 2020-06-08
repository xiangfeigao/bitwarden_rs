use diesel::prelude::*;
use serde_json::Value;

use crate::api::EmptyResult;
use crate::db::schema::org_policies;
use crate::db::DbConn;
use crate::error::MapResult;

use super::Organization;

#[derive(Debug, Identifiable, Queryable, Insertable, Associations, AsChangeset)]
#[table_name = "org_policies"]
#[belongs_to(Organization, foreign_key = "org_uuid")]
#[primary_key(uuid)]
pub struct OrgPolicy {
    pub uuid: String,
    pub org_uuid: String,
    pub atype: i32,
    pub enabled: bool,
    pub data: String,
}

#[allow(dead_code)]
#[derive(num_derive::FromPrimitive)]
pub enum OrgPolicyType {
    TwoFactorAuthentication = 0,
    MasterPassword = 1,
    PasswordGenerator = 2,
}

/// Local methods
impl OrgPolicy {
    pub fn new(org_uuid: String, atype: OrgPolicyType, data: String) -> Self {
        Self {
            uuid: crate::util::get_uuid(),
            org_uuid,
            atype: atype as i32,
            enabled: false,
            data,
        }
    }

    pub fn to_json(&self) -> Value {
        let data_json: Value = serde_json::from_str(&self.data).unwrap_or(Value::Null);
        json!({
            "Id": self.uuid,
            "OrganizationId": self.org_uuid,
            "Type": self.atype,
            "Data": data_json,
            "Enabled": self.enabled,
            "Object": "policy",
        })
    }
}

/// Database methods
impl OrgPolicy {
    #[cfg(feature = "postgresql")]
    pub fn save(&self, conn: &DbConn) -> EmptyResult {
        // We need to make sure we're not going to violate the unique constraint on org_uuid and atype.
        // This happens automatically on other DBMS backends due to replace_into(). PostgreSQL does
        // not support multiple constraints on ON CONFLICT clauses.
        diesel::delete(
            org_policies::table
                .filter(org_policies::org_uuid.eq(&self.org_uuid))
                .filter(org_policies::atype.eq(&self.atype)),
        )
        .execute(&**conn)
        .map_res("Error deleting org_policy for insert")?;

        diesel::insert_into(org_policies::table)
            .values(self)
            .on_conflict(org_policies::uuid)
            .do_update()
            .set(self)
            .execute(&**conn)
            .map_res("Error saving org_policy")
    }

    #[cfg(not(feature = "postgresql"))]
    pub fn save(&self, conn: &DbConn) -> EmptyResult {
        diesel::replace_into(org_policies::table)
            .values(&*self)
            .execute(&**conn)
            .map_res("Error saving org_policy")
    }

    pub fn delete(self, conn: &DbConn) -> EmptyResult {
        diesel::delete(org_policies::table.filter(org_policies::uuid.eq(self.uuid)))
            .execute(&**conn)
            .map_res("Error deleting org_policy")
    }

    pub fn find_by_uuid(uuid: &str, conn: &DbConn) -> Option<Self> {
        org_policies::table
            .filter(org_policies::uuid.eq(uuid))
            .first::<Self>(&**conn)
            .ok()
    }

    pub fn find_by_org(org_uuid: &str, conn: &DbConn) -> Vec<Self> {
        org_policies::table
            .filter(org_policies::org_uuid.eq(org_uuid))
            .load::<Self>(&**conn)
            .expect("Error loading org_policy")
    }

    pub fn find_by_user(user_uuid: &str, conn: &DbConn) -> Vec<Self> {
        use crate::db::schema::users_organizations;

        org_policies::table
            .left_join(
                users_organizations::table.on(
                    users_organizations::org_uuid.eq(org_policies::org_uuid)
                        .and(users_organizations::user_uuid.eq(user_uuid)))
            )
            .select(org_policies::all_columns)
            .load::<Self>(&**conn)
            .expect("Error loading org_policy")
    }

    pub fn find_by_org_and_type(org_uuid: &str, atype: i32, conn: &DbConn) -> Option<Self> {
        org_policies::table
            .filter(org_policies::org_uuid.eq(org_uuid))
            .filter(org_policies::atype.eq(atype))
            .first::<Self>(&**conn)
            .ok()
    }

    pub fn delete_all_by_organization(org_uuid: &str, conn: &DbConn) -> EmptyResult {
        diesel::delete(org_policies::table.filter(org_policies::org_uuid.eq(org_uuid)))
            .execute(&**conn)
            .map_res("Error deleting org_policy")
    }

    /*pub fn delete_all_by_user(user_uuid: &str, conn: &DbConn) -> EmptyResult {
        diesel::delete(twofactor::table.filter(twofactor::user_uuid.eq(user_uuid)))
            .execute(&**conn)
            .map_res("Error deleting twofactors")
    }*/
}
