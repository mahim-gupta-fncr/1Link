import secrets
from Crypto.Hash import keccak


def generate_secure_secret():
    """
    Generate a cryptographically secure 32-byte secret and its Keccak-256 hash.
    """
    # Generate 32 cryptographically secure random bytes
    secret_bytes = secrets.token_bytes(32)

    # Convert to hex string (equivalent to uint8ArrayToHex)
    secret_hex = secret_bytes.hex()

    return secret_hex

def generate_hashlock(secret_hex: str) -> str:
    """
    secret_hex: 64-character hex string (i.e. your 32 random bytes in hex form)
    returns: 64-character hex string of keccak256(secret_bytes)
    """
    secret_bytes = bytes.fromhex(secret_hex)
    k = keccak.new(digest_bits=256)
    k.update(secret_bytes)
    return k.hexdigest()
