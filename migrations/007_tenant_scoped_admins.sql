-- Per-tenant authorization.
--
-- Administration was authenticated but flat: every account was a platform account with
-- access to every business, every call and every provider credential, which meant a
-- business owner could not be given a login at all. `role` and `business_id` add the
-- authorization layer on top of the authentication that already existed.
--
-- The default is `platform`, so every account that exists today keeps exactly the access
-- it has now and no deployment changes behaviour on upgrade.
ALTER TABLE admin_users ADD COLUMN IF NOT EXISTS role text NOT NULL DEFAULT 'platform'
  CHECK (role IN ('platform', 'business'));

ALTER TABLE admin_users ADD COLUMN IF NOT EXISTS business_id uuid
  REFERENCES businesses(id) ON DELETE CASCADE;

-- The two roles are defined by what they are scoped to, so the database enforces it
-- rather than trusting every call site to remember: a business administrator must name
-- the business they administer, and a platform administrator must not be scoped to one.
ALTER TABLE admin_users DROP CONSTRAINT IF EXISTS admin_users_role_scope_check;
ALTER TABLE admin_users ADD CONSTRAINT admin_users_role_scope_check CHECK (
  (role = 'platform' AND business_id IS NULL) OR
  (role = 'business' AND business_id IS NOT NULL)
);

-- Listing the administrators of one business is the only scoped lookup this adds.
CREATE INDEX IF NOT EXISTS admin_users_business_idx ON admin_users (business_id);
