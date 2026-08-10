--!strict
--!native
-- Pure-Luau SHA-256 (FIPS 180-4), written for the bulk-transfer ops
-- (Design §7.4-A: `read_subtree` source manifests and `source_read`
-- verification are keyed on the SHA-256 of the exact captured bytes).
--
-- Clean-room implementation from the FIPS 180-4 specification — no vendored
-- code. No requires and no Roblox APIs, so the module loads under a
-- standalone `luau` binary and is unit-tested against the standard NIST
-- vectors (see the wave-4 test suite).

local Sha256 = {}

-- First 32 bits of the fractional parts of the cube roots of the first 64
-- primes (FIPS 180-4 §4.2.2)
local K = {
	0x428a2f98,
	0x71374491,
	0xb5c0fbcf,
	0xe9b5dba5,
	0x3956c25b,
	0x59f111f1,
	0x923f82a4,
	0xab1c5ed5,
	0xd807aa98,
	0x12835b01,
	0x243185be,
	0x550c7dc3,
	0x72be5d74,
	0x80deb1fe,
	0x9bdc06a7,
	0xc19bf174,
	0xe49b69c1,
	0xefbe4786,
	0x0fc19dc6,
	0x240ca1cc,
	0x2de92c6f,
	0x4a7484aa,
	0x5cb0a9dc,
	0x76f988da,
	0x983e5152,
	0xa831c66d,
	0xb00327c8,
	0xbf597fc7,
	0xc6e00bf3,
	0xd5a79147,
	0x06ca6351,
	0x14292967,
	0x27b70a85,
	0x2e1b2138,
	0x4d2c6dfc,
	0x53380d13,
	0x650a7354,
	0x766a0abb,
	0x81c2c92e,
	0x92722c85,
	0xa2bfe8a1,
	0xa81a664b,
	0xc24b8b70,
	0xc76c51a3,
	0xd192e819,
	0xd6990624,
	0xf40e3585,
	0x106aa070,
	0x19a4c116,
	0x1e376c08,
	0x2748774c,
	0x34b0bcb5,
	0x391c0cb3,
	0x4ed8aa4a,
	0x5b9cca4f,
	0x682e6ff3,
	0x748f82ee,
	0x78a5636f,
	0x84c87814,
	0x8cc70208,
	0x90befffa,
	0xa4506ceb,
	0xbef9a3f7,
	0xc67178f2,
}

--- SHA-256 of a byte string, as a 64-char lowercase hex digest.
function Sha256.hash(message: string): string
	local length = #message

	-- Padded message: original bytes + 0x80 + zeros + 64-bit big-endian bit
	-- length, to a multiple of 64 bytes
	local paddedLength = length + 9
	local remainder = paddedLength % 64

	if remainder ~= 0 then
		paddedLength += 64 - remainder
	end

	local padded = buffer.create(paddedLength)
	buffer.writestring(padded, 0, message)
	buffer.writeu8(padded, length, 0x80)

	-- 64-bit big-endian bit count. Luau numbers hold the < 2^53 sizes exactly;
	-- inputs here are bounded (≤32 MiB scripts) but the split stays general.
	local bits = length * 8
	local hi = math.floor(bits / 4294967296)
	local lo = bits % 4294967296

	buffer.writeu32(padded, paddedLength - 8, bit32.byteswap(hi))
	buffer.writeu32(padded, paddedLength - 4, bit32.byteswap(lo))

	-- Initial hash values (FIPS 180-4 §5.3.3)
	local h0, h1, h2, h3 = 0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a
	local h4, h5, h6, h7 = 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19

	local w = table.create(64, 0)

	local band = bit32.band
	local bxor = bit32.bxor
	local bnot = bit32.bnot
	local rrotate = bit32.rrotate
	local rshift = bit32.rshift
	local byteswap = bit32.byteswap
	local readu32 = buffer.readu32

	for blockOffset = 0, paddedLength - 64, 64 do
		-- Message schedule (buffer reads are little-endian; the wire words are
		-- big-endian, hence the byteswap)
		for t = 1, 16 do
			w[t] = byteswap(readu32(padded, blockOffset + (t - 1) * 4))
		end

		for t = 17, 64 do
			local w15 = w[t - 15]
			local w2 = w[t - 2]

			local s0 = bxor(rrotate(w15, 7), rrotate(w15, 18), rshift(w15, 3))
			local s1 = bxor(rrotate(w2, 17), rrotate(w2, 19), rshift(w2, 10))

			w[t] = (w[t - 16] + s0 + w[t - 7] + s1) % 4294967296
		end

		local a, b, c, d, e, f, g, h = h0, h1, h2, h3, h4, h5, h6, h7

		for t = 1, 64 do
			local S1 = bxor(rrotate(e, 6), rrotate(e, 11), rrotate(e, 25))
			local ch = bxor(band(e, f), band(bnot(e), g))
			local temp1 = (h + S1 + ch + K[t] + w[t]) % 4294967296
			local S0 = bxor(rrotate(a, 2), rrotate(a, 13), rrotate(a, 22))
			local maj = bxor(band(a, b), band(a, c), band(b, c))
			local temp2 = (S0 + maj) % 4294967296

			h = g
			g = f
			f = e
			e = (d + temp1) % 4294967296
			d = c
			c = b
			b = a
			a = (temp1 + temp2) % 4294967296
		end

		h0 = (h0 + a) % 4294967296
		h1 = (h1 + b) % 4294967296
		h2 = (h2 + c) % 4294967296
		h3 = (h3 + d) % 4294967296
		h4 = (h4 + e) % 4294967296
		h5 = (h5 + f) % 4294967296
		h6 = (h6 + g) % 4294967296
		h7 = (h7 + h) % 4294967296
	end

	return string.format("%08x%08x%08x%08x%08x%08x%08x%08x", h0, h1, h2, h3, h4, h5, h6, h7)
end

return Sha256
