-- vcd schema v3: whether a voice's stored reference went through the enrollment cleanup
-- (denoise and brightening), and whether a job's output goes through the finishing polish
-- (de-esser, loudness normalization, peak limiter). Rows from v2 were neither.
ALTER TABLE voices ADD COLUMN ref_cleaned INTEGER NOT NULL DEFAULT 0 CHECK (ref_cleaned IN (0, 1));
ALTER TABLE jobs ADD COLUMN polish INTEGER NOT NULL DEFAULT 0 CHECK (polish IN (0, 1));
