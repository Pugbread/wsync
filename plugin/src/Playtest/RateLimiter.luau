--!strict
-- Token-bucket rate limiter with explicit dropped accounting for the playtest
-- telemetry stream (playtest.json: "Events are not deduplicated or sampled by
-- the CLI; source rate limiting is visible through accurate `dropped`
-- counts"). The runtime agent charges one token per emit/log/signal frame;
-- when the bucket is empty the frame is counted, never silently discarded,
-- and the accumulated count is flushed as a `dropped` record the moment a
-- send succeeds again.
--
-- Sustained rate ~20 records/s with a burst of 40 (playtest.json "emit ~20/s
-- with burst"). Pure state machine with an injected clock — no requires, no
-- Roblox APIs — loadable under a standalone `luau` binary for behavior tests.

local RateLimiter = {}

RateLimiter.RATE = 20
RateLimiter.BURST = 40

export type Bucket = {
	rate: number,
	burst: number,
	tokens: number,
	lastRefill: number,
	dropped: number,
}

function RateLimiter.new(now: number, rate: number?, burst: number?): Bucket
	local effectiveBurst = burst or RateLimiter.BURST

	return {
		rate = rate or RateLimiter.RATE,
		burst = effectiveBurst,
		tokens = effectiveBurst,
		lastRefill = now,
		dropped = 0,
	}
end

local function refill(bucket: Bucket, now: number)
	local elapsed = now - bucket.lastRefill

	if elapsed > 0 then
		bucket.tokens = math.min(bucket.burst, bucket.tokens + elapsed * bucket.rate)
	end

	bucket.lastRefill = now
end

--- Try to take one token. Returns false — without charging — when the bucket
--- is empty; the caller then counts the drop instead of sending.
function RateLimiter.take(bucket: Bucket, now: number): boolean
	refill(bucket, now)

	if bucket.tokens < 1 then
		return false
	end

	bucket.tokens -= 1

	return true
end

--- Count records that were rate-limited away (or failed to send). Never
--- silent: the count is surfaced by the next successful `dropped` flush.
function RateLimiter.drop(bucket: Bucket, count: number?)
	bucket.dropped += math.max(1, math.floor(count or 1))
end

--- Claim the accumulated dropped count for one `dropped` record, resetting
--- the counter. Returns 0 when there is nothing to flush.
function RateLimiter.takeDropped(bucket: Bucket): number
	local count = bucket.dropped

	bucket.dropped = 0

	return count
end

--- Give a claimed count back after the `dropped` record itself failed to
--- send — the drops stay accounted for the next flush.
function RateLimiter.restoreDropped(bucket: Bucket, count: number)
	if count > 0 then
		bucket.dropped += count
	end
end

return RateLimiter
