"""One line of CSV."""


def parse_csv_line(line):
    """Split one CSV line into its fields (see the task's docstring)."""
    fields = []
    i, n = 0, len(line)
    while True:
        if i < n and line[i] == '"':
            i += 1
            value = []
            while True:
                if i >= n:
                    raise ValueError("unterminated quoted field")
                if line[i] == '"':
                    if i + 1 < n and line[i + 1] == '"':
                        value.append('"')
                        i += 2
                        continue
                    i += 1
                    break
                value.append(line[i])
                i += 1
            fields.append("".join(value))
            if i < n and line[i] != ",":
                raise ValueError("text after a closing quote")
        else:
            end = line.find(",", i)
            end = n if end == -1 else end
            fields.append(line[i:end])
            i = end
        if i >= n:
            return fields
        i += 1  # the comma
