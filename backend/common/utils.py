from decimal import Decimal
from Crypto.Hash import keccak
import base58

def near_to_yocto(near_amount):
    """Convert NEAR to yoctoNEAR (1 NEAR = 10^24 yoctoNEAR)"""
    return int(Decimal(near_amount) * 10**24)

def yocto_to_near(yocto_amount):
    """Convert yoctoNEAR to NEAR (1 yoctoNEAR = 10^-24 NEAR)"""
    return int(Decimal(yocto_amount) * 10**-24)



