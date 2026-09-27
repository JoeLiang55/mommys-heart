-- The volunteer's home address, now required at signup.
--
-- Defaulted to '' so rows predating this (including the volunteers backfilled by
-- 0015, who predate the agreement form entirely) stay valid: the requirement is
-- enforced when the form is submitted, not retroactively.
ALTER TABLE volunteers           ADD COLUMN home_address TEXT NOT NULL DEFAULT '';
ALTER TABLE volunteer_applicants ADD COLUMN home_address TEXT NOT NULL DEFAULT '';
