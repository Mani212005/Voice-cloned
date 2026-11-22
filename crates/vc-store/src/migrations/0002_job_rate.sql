-- vcd schema v2: a job's speaking rate in thousandths (1000 = the engine's own pace).
-- Jobs from v1 keep 1000, which is what they were accepted with.
ALTER TABLE jobs ADD COLUMN rate_milli INTEGER NOT NULL DEFAULT 1000
    CHECK (rate_milli BETWEEN 750 AND 1500);
