-- Issue 534: the authenticator secret is sealed to the sign-in (home tenant,
-- identity) instead of the tenant account that enrolled, so it opens in every
-- tenant of the person. No production users until 2027-01-01: secrets sealed
-- the old way are dropped and their owners enroll again.
DELETE FROM user_two_factor;
