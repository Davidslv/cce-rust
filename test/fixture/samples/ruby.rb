require "json"

def parse_config(text)
  JSON.parse(text)
end

class Loader
  def load(path)
    parse_config(File.read(path))
  end
end
