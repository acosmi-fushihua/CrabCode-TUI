package accountbridge

import (
	"strconv"
	"strings"
)

type releaseVersion struct {
	core       [3]uint64
	prerelease []string
}

func parseReleaseVersion(value string) (releaseVersion, bool) {
	if value == "" || len(value) > 128 || strings.Contains(value, "+") {
		return releaseVersion{}, false
	}
	parts := strings.SplitN(value, "-", 2)
	coreParts := strings.Split(parts[0], ".")
	if len(coreParts) != 3 {
		return releaseVersion{}, false
	}
	var parsed releaseVersion
	for index, part := range coreParts {
		if !validNumericIdentifier(part) {
			return releaseVersion{}, false
		}
		number, err := strconv.ParseUint(part, 10, 64)
		if err != nil {
			return releaseVersion{}, false
		}
		parsed.core[index] = number
	}
	if len(parts) == 1 {
		return parsed, true
	}
	identifiers := strings.Split(parts[1], ".")
	if len(identifiers) == 0 {
		return releaseVersion{}, false
	}
	for _, identifier := range identifiers {
		if identifier == "" {
			return releaseVersion{}, false
		}
		for _, character := range []byte(identifier) {
			if (character >= 'a' && character <= 'z') ||
				(character >= 'A' && character <= 'Z') ||
				(character >= '0' && character <= '9') || character == '-' {
				continue
			}
			return releaseVersion{}, false
		}
		if isDigits(identifier) && !validNumericIdentifier(identifier) {
			return releaseVersion{}, false
		}
	}
	parsed.prerelease = identifiers
	return parsed, true
}

func validNumericIdentifier(value string) bool {
	return value != "" && isDigits(value) && (len(value) == 1 || value[0] != '0')
}

func isDigits(value string) bool {
	if value == "" {
		return false
	}
	for _, character := range []byte(value) {
		if character < '0' || character > '9' {
			return false
		}
	}
	return true
}

func compareReleaseVersions(left, right releaseVersion) int {
	for index := range left.core {
		if left.core[index] < right.core[index] {
			return -1
		}
		if left.core[index] > right.core[index] {
			return 1
		}
	}
	if len(left.prerelease) == 0 && len(right.prerelease) == 0 {
		return 0
	}
	if len(left.prerelease) == 0 {
		return 1
	}
	if len(right.prerelease) == 0 {
		return -1
	}
	limit := len(left.prerelease)
	if len(right.prerelease) < limit {
		limit = len(right.prerelease)
	}
	for index := 0; index < limit; index++ {
		leftIdentifier := left.prerelease[index]
		rightIdentifier := right.prerelease[index]
		leftNumeric := isDigits(leftIdentifier)
		rightNumeric := isDigits(rightIdentifier)
		switch {
		case leftNumeric && rightNumeric:
			if len(leftIdentifier) < len(rightIdentifier) {
				return -1
			}
			if len(leftIdentifier) > len(rightIdentifier) {
				return 1
			}
		case leftNumeric:
			return -1
		case rightNumeric:
			return 1
		}
		if leftIdentifier < rightIdentifier {
			return -1
		}
		if leftIdentifier > rightIdentifier {
			return 1
		}
	}
	if len(left.prerelease) < len(right.prerelease) {
		return -1
	}
	if len(left.prerelease) > len(right.prerelease) {
		return 1
	}
	return 0
}
