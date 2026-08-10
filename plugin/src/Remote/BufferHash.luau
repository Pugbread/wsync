--!strict
-- SHA-256 hex digest of a buffer, for the transfer op families
-- (`capture_prepare`, `clipboard_copy`, `clipboard_paste_commit`). Uses
-- EncodingService's native `ComputeBufferHash` when the Studio build has it —
-- clipboard payloads reach 128 MiB, far past what pure Luau should chew —
-- and falls back to the pure Lib/Sha256 implementation otherwise, so the
-- digest is always available and always the same.

local WSync = script:FindFirstAncestor("WSync")

local Sha256 = require(WSync.Lib.Sha256)

local BufferHash = {}

local encodingService: any = nil

do
	local ok, service = pcall(function()
		return game:GetService("EncodingService")
	end)

	if ok then
		encodingService = service
	end
end

--- The lowercase SHA-256 hex digest of `bytes`.
function BufferHash.hex(bytes: buffer): string
	if encodingService ~= nil then
		local ok, digest = pcall(function()
			return encodingService:ComputeBufferHash(bytes, Enum.HashAlgorithm.Sha256) :: buffer
		end)

		if ok and typeof(digest) == "buffer" and buffer.len(digest) == 32 then
			local parts = table.create(32)

			for index = 0, 31 do
				parts[index + 1] = string.format("%02x", buffer.readu8(digest, index))
			end

			return table.concat(parts)
		end
	end

	return Sha256.hash(buffer.tostring(bytes))
end

return BufferHash
