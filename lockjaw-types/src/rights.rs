/// Access rights bitmask for handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Rights(u8);

pub const RIGHT_READ: u8 = 1 << 0;
pub const RIGHT_WRITE: u8 = 1 << 1;
pub const RIGHT_GRANT: u8 = 1 << 2;

impl Rights {
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// No rights.
    pub const fn none() -> Self {
        Self(0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let r = Rights::from_bits(RIGHT_READ | RIGHT_WRITE);
        assert_eq!(r.bits(), RIGHT_READ | RIGHT_WRITE);
    }

    #[test]
    fn none_is_zero() {
        assert_eq!(Rights::none().bits(), 0);
    }

    #[test]
    fn individual_bits() {
        assert_eq!(RIGHT_READ, 0b001);
        assert_eq!(RIGHT_WRITE, 0b010);
        assert_eq!(RIGHT_GRANT, 0b100);
    }

    #[test]
    fn combined_rights() {
        let rw = Rights::from_bits(RIGHT_READ | RIGHT_WRITE);
        assert_eq!(rw.bits() & RIGHT_READ, RIGHT_READ);
        assert_eq!(rw.bits() & RIGHT_WRITE, RIGHT_WRITE);
        assert_eq!(rw.bits() & RIGHT_GRANT, 0);
    }

    #[test]
    fn rights_check_logic() {
        // Simulate the handle_lookup rights check:
        // required.bits() & !slot.rights.bits() != 0 means "missing rights"
        let slot_rights = Rights::from_bits(RIGHT_READ | RIGHT_WRITE);
        let required_read = Rights::from_bits(RIGHT_READ);
        let required_grant = Rights::from_bits(RIGHT_GRANT);

        // Read should pass
        assert_eq!(required_read.bits() & !slot_rights.bits(), 0);
        // Grant should fail
        assert_ne!(required_grant.bits() & !slot_rights.bits(), 0);
    }
}
