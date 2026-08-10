--!strict
-- Ref hex codec for the protocol-v1 boundary (Design §4.1).
--
-- Core keeps using Argon's 16-byte buffer Refs internally; the JSON wire
-- format encodes every Ref as a 32-char lowercase hex string. Conversion
-- happens exclusively at the transport boundary (Transport/WebSocket), so
-- nothing above the transport ever sees a hex ref.

local Hex = {}

-- The all-zeros ref is the "root" sentinel (Design §4.1)
Hex.ZERO = string.rep("0", 32)

local BYTE_TO_HEX: { [number]: string } = {}
for byte = 0, 255 do
	BYTE_TO_HEX[byte] = string.format("%02x", byte)
end

--- Encode a 16-byte buffer Ref as a 32-char lowercase hex string
function Hex.encode(ref: buffer): string
	local out = table.create(16)

	for i = 0, 15 do
		out[i + 1] = BYTE_TO_HEX[buffer.readu8(ref, i)]
	end

	return table.concat(out)
end

--- Decode a 32-char hex string into a 16-byte buffer Ref, or nil when malformed
function Hex.decode(hex: any): buffer?
	if type(hex) ~= "string" or #hex ~= 32 or not hex:match("^[0-9a-fA-F]+$") then
		return nil
	end

	local buf = buffer.create(16)

	for i = 0, 15 do
		buffer.writeu8(buf, i, tonumber(hex:sub(i * 2 + 1, i * 2 + 2), 16) :: number)
	end

	return buf
end

return Hex
