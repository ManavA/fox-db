class Greeter
  def greet(who)
    format_name(who)
  end
end

def format_name(s)
  "Hello #{s}"
end

def main
  Greeter.new.greet("x")
end
