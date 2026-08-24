-- Badges are public by design (they end up in READMEs and dashboards), so they
-- must not be addressed by the check uuid: that uuid is the ping credential,
-- and anyone who reads a badge URL could then forge pings.

ALTER TABLE checks ADD COLUMN badge_token TEXT NOT NULL DEFAULT '';

UPDATE checks SET badge_token = lower(hex(randomblob(12))) WHERE badge_token = '';

CREATE UNIQUE INDEX checks_badge_token ON checks (badge_token);
