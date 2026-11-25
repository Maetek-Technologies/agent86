"""One line of CSV."""


def parse_csv_line(line):
    """Split one CSV line into its fields, a list of strings.

    - Fields are separated by commas. Spaces are part of a field.
    - A field may be quoted with double quotes. Inside quotes, commas are
      literal, and two double quotes ("") stand for one double quote.
    - A quoted field ends at its closing quote; the next character must be a
      comma or the end of the line, otherwise ValueError.
    - A quote that opens a field and is never closed raises ValueError.
    - A double quote inside an unquoted field is an ordinary character.
    - An empty line is one empty field, and a trailing comma means a last
      empty field.
    """
    raise NotImplementedError
